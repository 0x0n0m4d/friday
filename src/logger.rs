//! Log JSONL no diretório de estado: rotação por tamanho, retenção por dias,
//! eventos (nada de texto do modelo, prompts ou transcrições).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

const FILE_PREFIX: &str = "sexta-feira";

pub struct Logger {
    inner: Mutex<LoggerInner>,
    journal_dir: PathBuf,
}

struct LoggerInner {
    file: Option<fs::File>,
    day: u64,
    bytes: u64,
}

fn day_number() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400
}

impl Logger {
    pub fn open(journal_dir: &Path) -> Logger {
        let journal = journal_dir.to_path_buf();
        let _ = fs::create_dir_all(&journal);
        Logger {
            inner: Mutex::new(LoggerInner {
                file: None,
                day: 0,
                bytes: 0,
            }),
            journal_dir: journal,
        }
    }

    /// Aplica retenção: remove arquivos de dias antigos; fecha o arquivo
    /// corrente se passou do teto de bytes (o próximo registro já abre outro).
    pub fn prune(&self, retain_days: u64, max_file_bytes: u64) {
        let cutoff = day_number().saturating_sub(retain_days);
        if let Ok(entries) = fs::read_dir(&self.journal_dir) {
            for e in entries.flatten() {
                let p = e.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let Some(rest) = name
                    .strip_prefix(&format!("{FILE_PREFIX}."))
                    .and_then(|r| r.strip_suffix(".jsonl"))
                else {
                    continue;
                };
                if let Ok(day) = rest.parse::<u64>() {
                    if day < cutoff {
                        let _ = fs::remove_file(&p);
                    }
                }
            }
        }
        if let Ok(mut g) = self.inner.lock() {
            if g.bytes >= max_file_bytes {
                g.file = None;
            }
        }
    }

    pub fn log_event(&self, event: &str, fields: &[(&str, Value)]) {
        let mut record = json!({
            "ts": ts_iso8601(),
            "event": event,
        });
        if let Some(obj) = record.as_object_mut() {
            for (k, v) in fields {
                obj.insert((*k).into(), redact(v));
            }
        }
        self.write_line(&record.to_string());
    }

    pub fn log_error(&self, event: &str, reason: &str) {
        self.log_event(event, &[("reason", json!(reason))]);
    }

    fn write_line(&self, line: &str) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let today = day_number();
        if g.file.is_none() || g.day != today {
            let path = path_for(&self.journal_dir, today);
            let Ok(f) = fs::OpenOptions::new().create(true).append(true).open(&path) else {
                return;
            };
            g.file = Some(f);
            g.day = today;
            g.bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        }
        if let Some(f) = g.file.as_mut() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.write_all(b"\n");
            g.bytes += line.len() as u64 + 1;
        }
    }
}

fn path_for(base: &Path, day: u64) -> PathBuf {
    base.join(format!("{FILE_PREFIX}.{day}.jsonl"))
}

/// Timestamp ISO-8601 (UTC) sem dependência extra.
fn ts_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil_from_secs(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Converte Unix epoch em elementos civis (algoritmo de Howard Hinnant).
fn civil_from_secs(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let h = rem / 3600;
    let mi = (rem % 3600) / 60;
    let s = rem % 60;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    if m <= 2 {
        y += 1;
    }
    (y, m, d, h, mi, s)
}

/// No log nada é segredo, mas por garantia: valores que pareçam tokens
/// hexadecimais longos são redigidos.
pub fn redact(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(
            if s.len() >= 16
                && s.chars()
                    .all(|c| c.is_ascii_hexdigit() || c == '_' || c == '-')
            {
                "<segredo redigido>".into()
            } else {
                s.clone()
            },
        ),
        Value::Array(a) => Value::Array(a.iter().map(redact).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, val)| (k.clone(), redact(val))).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn grava_jsonl_com_evento() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("journal");
        let log = Logger::open(&dir);
        log.log_event("sessao", &[("agent", json!("casa"))]);
        log.log_event("ptt", &[("fase", json!("press"))]);
        let mut found = 0;
        for e in fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                let txt = fs::read_to_string(&p).unwrap();
                assert!(txt.contains("\"event\":\"sessao\""));
                assert!(txt.contains("\"event\":\"ptt\""));
                assert!(!txt.to_lowercase().contains("transcri"));
                found += 1;
            }
        }
        assert_eq!(found, 1, "um arquivo por dia");
    }

    #[test]
    fn podagem_change_minutos_retidos() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("journal");
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join(format!("{FILE_PREFIX}.1.jsonl"));
        fs::write(&old, "{}").unwrap();
        let log = Logger::open(&dir);
        log.prune(30, 1024);
        assert!(!old.exists());
    }

    #[test]
    fn data_civil() {
        assert_eq!(civil_from_secs(0), (1970, 1, 1, 0, 0, 0));
        let ts = ts_iso8601();
        assert!(ts.ends_with('Z') && ts.len() == 20, "ts = {ts}");
    }

    #[test]
    fn redige_segredos() {
        assert_eq!(
            redact(&json!({"tok": "a1b2c3d4-e5f6-a7b8-c9d0-e1f2a3b4c5d6"})),
            json!({"tok": "<segredo redigido>"})
        );
        assert_eq!(redact(&json!("casa")), json!("casa"));
    }
}
