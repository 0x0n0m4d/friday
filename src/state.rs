//! Estado em disco: diretório de estado, trava de instância única e a
//! associação agente → sessão (id), sempre com permissão 0600.

use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Diretório de estado padrão: XDG state ou ~/.local/state/sexta-feira.
pub fn default_state_dir() -> Option<PathBuf> {
    let base = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("sexta-feira"))
}

pub struct StateDir {
    pub root: PathBuf,
}

#[derive(Debug)]
pub struct StateError(String);

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StateError {}

impl StateDir {
    /// Cria o diretório com modo 0700 (não muda se já existir com 0700 sanável).
    pub fn open(root: PathBuf) -> Result<StateDir, StateError> {
        fs::create_dir_all(&root)
            .map_err(|e| StateError(format!("não foi possível criar {}: {e}", root.display())))?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(|e| {
            StateError(format!(
                "não foi possível restringir {}: {e}",
                root.display()
            ))
        })?;
        fs::metadata(&root)
            .map_err(|e| StateError(format!("sem metadados de {}: {e}", root.display())))
            .and_then(|m| {
                if m.is_dir() {
                    Ok(())
                } else {
                    Err(StateError(format!("{} não é um diretório", root.display())))
                }
            })?;
        Ok(StateDir { root })
    }

    pub fn journal_dir(&self) -> PathBuf {
        self.root.join("journal")
    }

    pub fn tmp_dir(&self) -> Result<PathBuf, StateError> {
        let d = self.root.join("tmp");
        fs::create_dir_all(&d)
            .map_err(|e| StateError(format!("não foi possível criar {}: {e}", d.display())))?;
        fs::set_permissions(&d, fs::Permissions::from_mode(0o700))
            .map_err(|e| StateError(format!("não foi possível restringir {}: {e}", d.display())))?;
        Ok(d)
    }

    /// Trava de instância única via flock (bloqueia uma segunda execução).
    pub fn try_lock(&self) -> Result<InstanceLock, StateError> {
        let path = self.root.join("sexta-feira.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| StateError(format!("não foi possível abrir {}: {e}", path.display())))?;
        unsafe {
            // SAFETY: flock clássico em arquivo própio; libc direta.
            let rc = libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB);
            if rc != 0 {
                return Err(StateError(
                    "outra instância do sexta-feira já está em execução \
                     (trava em sextadir/sexta-feira.lock)."
                        .replace("sextadir", &self.root.display().to_string()),
                ));
            }
        }
        Ok(InstanceLock { _file: file, path })
    }

    pub fn save_session(&self, alias: &str, session_id: &str) -> Result<(), StateError> {
        let p = self.session_path(alias);
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let body = serde_json::json!({ "session_id": session_id, "saved_at_secs": secs });
        let tmp = p.with_extension("tmp");
        {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| {
                    StateError(format!("não foi possível abrir {}: {e}", tmp.display()))
                })?;
            f.write_all(body.to_string().as_bytes())
                .map_err(|e| StateError(format!("falha ao gravar {}: {e}", tmp.display())))?;
        }
        fs::rename(&tmp, &p).map_err(|e| StateError(format!("renomeio falhou: {e}")))?;
        Ok(())
    }

    pub fn load_session(&self, alias: &str) -> Result<Option<String>, StateError> {
        let p = self.session_path(alias);
        if !p.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(&p)
            .map_err(|e| StateError(format!("não foi possível ler {}: {e}", p.display())))?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| StateError(format!("{} não é JSON válido: {e}", p.display())))?;
        match v.get("session_id").and_then(|s| s.as_str()) {
            Some(id) if !id.is_empty() => Ok(Some(id.to_string())),
            _ => Err(StateError(format!("{} não contém session_id", p.display()))),
        }
    }

    /// Apaga registros de sessão quando app encerra por --new-session.
    pub fn clear_session(&self, alias: &str) {
        let p = self.session_path(alias);
        let _ = fs::remove_file(p);
    }

    fn session_path(&self, alias: &str) -> PathBuf {
        self.root.join(format!("sessao-{alias}.json"))
    }
}

pub struct InstanceLock {
    _file: fs::File,
    #[allow(dead_code)]
    path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn abre_com_0700_e_guarda_sessao() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::open(tmp.path().join("estado")).unwrap();
        assert_eq!(
            fs::metadata(&dir.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        dir.save_session("trabalho", "s-abc").unwrap();
        let p = dir.session_path("trabalho");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            dir.load_session("trabalho").unwrap().as_deref(),
            Some("s-abc")
        );
    }

    #[test]
    fn trava_unica() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::open(tmp.path().join("estado")).unwrap();
        let _l1 = dir.try_lock().unwrap();
        let l2 = dir.try_lock();
        assert!(l2.is_err(), "segunda trava deve falhar");
    }

    #[test]
    fn sessao_apagada() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::open(tmp.path().join("estado")).unwrap();
        dir.save_session("casa", "s-1").unwrap();
        dir.clear_session("casa");
        assert_eq!(dir.load_session("casa").unwrap(), None);
    }
}
