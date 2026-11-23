//! Gravador por comando externo: escreve WAV mono/16 kHz/PCM16 em um
//! diretório temporário próprio (0700, arquivos 0600) e nunca deixa o
//! processo filho rodando se o programa for interrompido.

use std::fmt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

use crate::wav::parse_wav;

#[derive(Debug)]
pub struct RecorderError(String);

impl fmt::Display for RecorderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for RecorderError {}

#[derive(Debug)]
pub struct Recording {
    pub path: PathBuf,
}

#[derive(Debug)]
pub enum RecorderEvent {
    /// Gravação encerrada (por parada, prazo ou fim do comando) e validada.
    Done(Result<Recording, RecorderError>),
}

pub struct ActiveRecording {
    stop: Option<Sender<()>>,
    pub path: PathBuf,
}

impl ActiveRecording {
    /// Para a gravação de forma assíncrona; o resultado chega pelo canal de
    /// eventos como `RecorderEvent::Done`.
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for ActiveRecording {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Inicia o comando de gravação apontando um WAV temporário.
/// A permissão do arquivo é 0600; o diretório precisa existir com 0700.
pub fn start_recording(
    command: &[String],
    max_secs: u64,
    tmp_dir: &Path,
    events: Sender<RecorderEvent>,
) -> Result<ActiveRecording, RecorderError> {
    let path = tmp_dir.join(format!(
        "gravacao-{}.wav",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| RecorderError(format!("não foi possível criar {}: {e}", path.display())))?;

    if command.is_empty() || command[0].trim().is_empty() {
        return Err(RecorderError("comando de gravação vazio".into()));
    }
    let mut parts: Vec<String> = Vec::with_capacity(command.len());
    let mut replaced = false;
    for a in command {
        if a.contains("{file}") {
            parts.push(a.replace("{file}", &path.display().to_string()));
            replaced = true;
        } else {
            parts.push(a.clone());
        }
    }
    if !replaced {
        return Err(RecorderError(
            "comando de gravação sem {file} — configuração inválida".into(),
        ));
    }

    let (stop_tx, stop_rx) = channel::<()>();
    let thread_path = path.clone();
    let recorder_command = parts;
    let _join = std::thread::Builder::new()
        .name("recorder".into())
        .spawn(move || {
            let out = run_recorder(&recorder_command, &thread_path, max_secs, stop_rx);
            let _ = events.send(RecorderEvent::Done(out));
        })
        .map_err(|e| RecorderError(format!("falha ao iniciar fio de gravação: {e}")))?;
    Ok(ActiveRecording {
        stop: Some(stop_tx),
        path,
    })
}

fn validate_recording(path: &Path) -> Result<Recording, RecorderError> {
    let bytes = std::fs::read(path)
        .map_err(|e| RecorderError(format!("falha ao ler {}: {e}", path.display())))?;
    match parse_wav(&bytes, crate::wav::MAX_WAV_READ_BYTES) {
        Ok(info) => match info.check_recording() {
            Ok(()) => Ok(Recording {
                path: path.to_path_buf(),
            }),
            Err(e) => Err(RecorderError(format!("WAV de gravação inválido: {e}"))),
        },
        Err(e) => Err(RecorderError(format!("gravação não é um WAV válido: {e}"))),
    }
}

fn run_recorder(
    command: &[String],
    path: &Path,
    max_secs: u64,
    stop_rx: std::sync::mpsc::Receiver<()>,
) -> Result<Recording, RecorderError> {
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let mut child = unsafe {
        // SAFETY: cria grupo próprio para o pipeline inteiro poder ser
        // morto depois; API documentada do std (pre_exec, unix).
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        })
    }
    .spawn()
    .map_err(|e| RecorderError(format!("falha ao iniciar gravador {:?}: {e}", command[0])))?;
    let pgid = child.id() as i32;

    let deadline = Instant::now() + Duration::from_secs(max_secs);
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    stop_group_gracefully(pgid, &mut child);
    let _ = child.wait();
    if let Err(e) = repair_streaming_header(path) {
        return Err(RecorderError(format!(
            "não consegui corrigir o cabeçalho da gravação: {e}"
        )));
    }
    validate_recording(path)
}

/// Fecha o que o gravador deixou aberto: enquanto gravam, arecord (e
/// similares) vendem o chunk `data` com marcador de tamanho infinito
/// (0x80000000) e só substituem pelo tamanho real ao sair. Se o fechamento
/// gracioso não acontecer (drain travado, KILL, gravador que ignora
/// SIGTERM), o cabeçalho fica mentiroso com corpo real e válido. Aqui
/// escrevemos o tamanho verdadeiro. No-op quando o cabeçalho já é final ou
/// o layout não é o clássico — nesse caso o parser decide.
fn repair_streaming_header(path: &Path) -> Result<(), std::io::Error> {
    let mut bytes = std::fs::read(path)?;
    if bytes.len() <= 44 {
        return Ok(());
    }
    let le32 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    const DATA_MARKERS: [u32; 3] = [0x8000_0000, 0x7FFF_FFFF, 0xFFFF_FFFF];
    if &bytes[8..12] != b"WAVE" || &bytes[12..16] != b"fmt " {
        return Ok(());
    }
    let fmt_size = le32(&bytes[16..20]) as usize;
    if fmt_size < 16 {
        return Ok(());
    }
    let data_at = 12 + 8 + fmt_size + (fmt_size % 2);
    if data_at + 8 > bytes.len() || &bytes[data_at..data_at + 4] != b"data" {
        return Ok(());
    }
    let declared = le32(&bytes[data_at + 4..data_at + 8]);
    if !DATA_MARKERS.contains(&declared) {
        return Ok(());
    }
    let body = bytes.len() - (data_at + 8);
    if body == 0 || body as u64 > declared as u64 {
        return Ok(());
    }
    let total = bytes.len();
    bytes[data_at + 4..data_at + 8].copy_from_slice(&(body as u32).to_le_bytes());
    bytes[4..8].copy_from_slice(&((total - 8) as u32).to_le_bytes());
    std::fs::write(path, &bytes)
}

/// Para o grupo: SIGTERM e esperança de o gravador fechar o WAV com
/// cabeçalho corrigido (arecord/pw-record fazem isso ao sair); só então
/// SIGKILL. O KILL cedo trunca o arquivo e o parser rejeita (data declara
/// mais bytes do que o corpo tem).
fn stop_group_gracefully(pgid: i32, child: &mut std::process::Child) {
    unsafe {
        // SAFETY: sinal para o grupo criado com setpgid(0,0) no filho.
        libc::kill(-pgid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}

/// Após o uso o WAV temporário é removido (arquivos 0600 em diretório 0700).
pub fn remove_recording(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn make_wav16k(rate: u32, channels: u16) -> Vec<u8> {
        let data_len = 2usize;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data_len as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 2]);
        out
    }

    #[test]
    fn sem_placeholder_rejeita() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, _) = std::sync::mpsc::channel();
        let r = start_recording(&["/bin/cat".to_string()], 2, tmp.path(), tx);
        assert!(r.is_err());
    }

    #[test]
    fn grava_para_e_valida_com_script_falso() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("fix.wav");
        std::fs::write(&fixture, make_wav16k(16_000, 1)).unwrap();
        let script = format!("cp '{0}' \"$0\"; sleep 30", fixture.display());
        let (tx, rx) = std::sync::mpsc::channel::<RecorderEvent>();
        let mut handle = start_recording(
            &[
                "/bin/sh".to_string(),
                "-c".to_string(),
                script,
                "{file}".to_string(),
            ],
            5,
            tmp.path(),
            tx,
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&handle.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let wait_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let size = std::fs::metadata(&handle.path)
                .map(|m| m.len())
                .unwrap_or(0);
            if size >= 46 || Instant::now() >= wait_deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        handle.stop();
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(RecorderEvent::Done(Ok(rec))) => {
                assert!(rec.path.exists());
                remove_recording(&rec.path);
                assert!(!rec.path.exists());
            }
            other => panic!("resultado inesperado: {other:?}"),
        }
    }

    #[test]
    fn gravação_estéreo_validada_erro() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.wav");
        std::fs::write(&path, make_wav16k(16_000, 2)).unwrap();
        let err = validate_recording(&path).unwrap_err();
        assert!(err.to_string().contains("mono"));
    }
}
