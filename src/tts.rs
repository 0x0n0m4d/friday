//! Voz: prepara o texto do modelo para fala (filter), enfileira em frases e
//! toca com o Piper nativo + um reprodutor configurado. Falhas nunca são
//! silenciosas: aparecem na barra de status (sem nunca falar o texto).

use std::fmt;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::wav::parse_wav;

#[derive(Debug)]
pub struct TtsError(pub String);

impl TtsError {
    pub fn is_interrupted(&self) -> bool {
        self.0 == "interrompido"
    }
}

impl fmt::Display for TtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TtsError {}

pub enum TtsEvent {
    Started,
    Finished(Result<(), TtsError>),
    Interrupted,
}

enum SpeakerCommand {
    Speak(String),
    Interrupt,
    Shutdown,
}

pub struct SpeakerConfig {
    pub piper_command: Vec<String>,
    /// "stdout": WAV na saída padrão do piper; "file": piper grava em {file}.
    pub piper_output: String,
    pub player_command: Vec<String>,
    pub timeout_secs: u64,
    pub max_wav_bytes: usize,
    pub tmp_dir: PathBuf,
}

struct Fragment {
    bytes: Vec<u8>,
    #[allow(dead_code)]
    sample_rate: u32,
    #[allow(dead_code)]
    channels: u16,
}

/// Remove elementos de Markdown e coisas que não devem ser faladas, mas
/// preserva identificadores como `snake_case` intactos.
pub fn prepare_for_speech(text: &str) -> Vec<String> {
    let cleaned = strip_speech_noise(text);
    split_sentences(&cleaned)
}

fn strip_speech_noise(text: &str) -> String {
    let mut out = String::new();
    let mut bytes = text.chars();
    let mut bytes_line: Vec<char> = Vec::new();
    let mut fenced = false;
    while let Some(c) = bytes.next() {
        match c {
            '`' => {
                let is_fence = {
                    let probe: String = bytes.clone().take(2).collect();
                    probe == "``"
                };
                if is_fence {
                    bytes.next();
                    bytes.next();
                    fenced = !fenced;
                    bytes_line.clear();
                } else {
                    // código inline: mantém o conteúdo, tira as crases
                    for c2 in bytes.by_ref() {
                        if c2 == '`' {
                            break;
                        }
                        bytes_line.push(c2);
                    }
                }
            }
            '\n' => {
                let line: String = bytes_line.iter().collect();
                let trimmed = line.trim();
                if !fenced && !is_table_line(trimmed) {
                    out.push_str(trimmed);
                    out.push('\n');
                }
                bytes_line.clear();
            }
            _ => bytes_line.push(c),
        }
    }
    let rest: String = bytes_line.iter().collect();
    let trimmed = rest.trim();
    if !trimmed.is_empty() && !is_table_line(trimmed) {
        out.push_str(trimmed);
        out.push('\n');
    }
    let mut final_text = String::new();
    for line in out.lines() {
        let line = line
            .replace("**", "")
            .replace("__", "")
            .trim_start_matches(['#', '>', '*', '-', '+', ' '])
            .trim()
            .to_string();
        if line.is_empty() {
            continue;
        }
        final_text.push_str(&remove_urls_and_paths(&line));
        final_text.push(' ');
    }
    final_text
}

fn is_table_line(line: &str) -> bool {
    let t = line.trim();
    if t.starts_with('|') {
        return true;
    }
    if t.starts_with("+-") || t.starts_with("=+") {
        return true;
    }
    t.split_whitespace().count() >= 2 && t.matches('|').count() >= 2
}

fn remove_urls_and_paths(line: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for token in line.split_whitespace() {
        let is_url = token.starts_with("http://")
            || token.starts_with("https://")
            || token.starts_with("www.");
        let is_path = token.starts_with('/') && token.matches('/').count() >= 2;
        if is_url || is_path {
            continue;
        }
        out.push(token);
    }
    out.join(" ")
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        current.push(c);
        match c {
            '.' | '!' | '?' | '\u{2026}' => {
                while matches!(chars.peek(), Some('.' | '!') | Some('?')) {
                    current.push(chars.next().unwrap());
                }
                if chars.peek().is_none_or(|n| n.is_whitespace()) {
                    let found = current.trim().to_string();
                    if !found.is_empty() {
                        sentences.push(found);
                    }
                    current.clear();
                }
            }
            '\n' => {
                let found = current.trim().to_string();
                if !found.is_empty() {
                    sentences.push(found);
                }
                current.clear();
            }
            _ => {}
        }
    }
    if !current.trim().is_empty() {
        sentences.push(current.trim().to_string());
    }
    sentences.into_iter().filter(|s| !s.is_empty()).collect()
}

// ---------- fio do falante ----------

pub struct Speaker {
    tx: Mutex<std::sync::mpsc::Sender<SpeakerCommand>>,
    cancel: Arc<AtomicBool>,
}

impl Speaker {
    pub fn new(cfg: SpeakerConfig, events: Sender<TtsEvent>) -> Speaker {
        let (tx, rx) = std::sync::mpsc::channel::<SpeakerCommand>();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = cancel.clone();
        let _ = std::thread::Builder::new()
            .name("speaker".into())
            .spawn(move || {
                speaker_loop(cfg, rx, events, cancel_thread);
            });
        Speaker {
            tx: Mutex::new(tx),
            cancel,
        }
    }

    /// Enfileira uma frase; a ordem é preservada.
    pub fn speak(&self, sentence: String) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(SpeakerCommand::Speak(sentence));
        }
    }

    /// Interrompe imediatamente o que está sendo falado e esvazia a fila.
    pub fn interrupt(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(SpeakerCommand::Interrupt);
        }
    }

    pub fn shutdown(&self) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(SpeakerCommand::Shutdown);
        }
    }
}

fn speaker_loop(
    cfg: SpeakerConfig,
    rx: std::sync::mpsc::Receiver<SpeakerCommand>,
    events: Sender<TtsEvent>,
    cancel: Arc<AtomicBool>,
) {
    let mut queue: Vec<String> = Vec::new();
    'main: loop {
        match rx.recv() {
            Ok(SpeakerCommand::Shutdown) => break,
            Ok(SpeakerCommand::Interrupt) => {
                queue.clear();
                let _ = events.send(TtsEvent::Interrupted);
                cancel.store(false, Ordering::SeqCst);
            }
            Ok(SpeakerCommand::Speak(s)) => queue.push(s),
            Err(_) => break,
        }
        while let Some(sentence) = queue.first().cloned() {
            if cancel.load(Ordering::SeqCst) {
                cancel.store(false, Ordering::SeqCst);
                queue.clear();
                let _ = events.send(TtsEvent::Interrupted);
                continue 'main;
            }
            let _ = events.send(TtsEvent::Started);
            match render_and_play(&cfg, &sentence, &cancel) {
                Ok(()) => {
                    let _ = events.send(TtsEvent::Finished(Ok(())));
                }
                Err(e) if e.is_interrupted() => {
                    queue.clear();
                    let _ = events.send(TtsEvent::Interrupted);
                    continue 'main;
                }
                Err(e) => {
                    let _ = events.send(TtsEvent::Finished(Err(e)));
                }
            }
            queue.remove(0);
        }
    }
}

fn render_and_play(
    cfg: &SpeakerConfig,
    sentence: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<(), TtsError> {
    if cfg.piper_command.is_empty() {
        return Err(TtsError("comando do piper não configurado".into()));
    }
    if cfg.player_command.is_empty() {
        return Err(TtsError("comando do reprodutor não configurado".into()));
    }
    let fragment = render_fragment(cfg, sentence, cancel)?;
    play_fragment(cfg, fragment, cancel)
}

fn tmp_wav_path(cfg: &SpeakerConfig) -> Result<PathBuf, TtsError> {
    let tmp = cfg.tmp_dir.join(format!(
        "fala-{}.wav",
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
        .open(&tmp)
        .map_err(|e| TtsError(format!("não foi possível criar {}: {e}", tmp.display())))?;
    Ok(tmp)
}

/// Roda o piper e coleta os bytes do WAV, com teto de tamanho aplicado
/// durante a leitura e validação do cabeçalho assim que há chunk fmt.
fn render_fragment(
    cfg: &SpeakerConfig,
    sentence: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<Fragment, TtsError> {
    let file_mode = cfg.piper_output == "file";
    let mut parts: Vec<String> = Vec::with_capacity(cfg.piper_command.len());
    let mut temp_path: Option<PathBuf> = None;
    for a in &cfg.piper_command {
        if file_mode && a.contains("{file}") {
            let tmp = tmp_wav_path(cfg)?;
            temp_path = Some(tmp.clone());
            parts.push(a.replace("{file}", &tmp.display().to_string()));
        } else {
            parts.push(a.clone());
        }
    }
    let mut child = unsafe {
        // SAFETY: grupo próprio para interrupção imediata de todo o comando.
        Command::new(&parts[0])
            .args(&parts[1..])
            .stdin(Stdio::piped())
            .stdout(if file_mode {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stderr(Stdio::null())
            .pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            })
    }
    .spawn()
    .map_err(|e| TtsError(format!("falha ao iniciar o piper: {e}")))?;
    let pgid = child.id() as i32;

    if let Some(mut stdin) = child.stdin.take() {
        let to_write = sentence.as_bytes().to_vec();
        let cancel_flag = cancel.clone();
        let _ = std::thread::Builder::new()
            .name("piper-feed".into())
            .spawn(move || {
                if !cancel_flag.load(Ordering::SeqCst) {
                    let _ = stdin.write_all(&to_write);
                }
            });
    }

    let result = collect_piper_output(cfg, &mut child, temp_path.as_deref(), cancel);
    kill_group(pgid);
    let _ = child.wait();
    if let Some(tmp) = &temp_path {
        let _ = std::fs::remove_file(tmp);
    }
    let raw = result?;
    if raw.is_empty() {
        return Err(TtsError("o piper não produziu áudio".into()));
    }
    let info = parse_wav(&raw, cfg.max_wav_bytes)
        .map_err(|e| TtsError(format!("áudio do piper rejeitado: {e}")))?;
    info.check_playback()
        .map_err(|e| TtsError(format!("áudio do piper rejeitado: {e}")))?;
    Ok(Fragment {
        bytes: raw,
        sample_rate: info.sample_rate,
        channels: info.channels,
    })
}

fn collect_piper_output(
    cfg: &SpeakerConfig,
    child: &mut std::process::Child,
    temp_path: Option<&Path>,
    cancel: &Arc<AtomicBool>,
) -> Result<Vec<u8>, TtsError> {
    let deadline = Instant::now() + Duration::from_secs(cfg.timeout_secs);
    if let Some(path) = temp_path {
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(TtsError("interrompido".into()));
            }
            if Instant::now() >= deadline {
                return Err(TtsError(format!(
                    "piper não terminou em {}s",
                    cfg.timeout_secs
                )));
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => return Err(TtsError(format!("espera do piper falhou: {e}"))),
            }
        }
        let out = std::fs::read(path)
            .map_err(|e| TtsError(format!("falha ao ler {}.wav de saída: {e}", path.display())))?;
        if out.len() > cfg.max_wav_bytes {
            return Err(TtsError(format!(
                "áudio do piper passou do teto de {} bytes",
                cfg.max_wav_bytes
            )));
        }
        Ok(out)
    } else {
        let mut out: Vec<u8> = Vec::new();
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| TtsError("sem saída padrão do piper".into()))?;
        let mut buf = [0u8; 4096];
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(TtsError("interrompido".into()));
            }
            if Instant::now() >= deadline {
                return Err(TtsError(format!(
                    "piper não terminou em {}s",
                    cfg.timeout_secs
                )));
            }
            if out.len() >= 44 && out.len() < 104 {
                if let Ok(info) = parse_wav(&out, cfg.max_wav_bytes) {
                    if let Err(e) = info.check_playback() {
                        return Err(TtsError(format!("áudio do piper rejeitado: {e}")));
                    }
                }
            }
            if out.len() > cfg.max_wav_bytes {
                return Err(TtsError(format!(
                    "áudio do piper passou do teto de {} bytes",
                    cfg.max_wav_bytes
                )));
            }
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(TtsError(format!("leitura do piper falhou: {e}"))),
            }
        }
        Ok(out)
    }
}

/// O reprodutor recebe os bytes por `/dev/stdin` (o `{file}` no comando do
/// reprodutor é substituído por `/dev/stdin`).
fn play_fragment(
    cfg: &SpeakerConfig,
    fragment: Fragment,
    cancel: &Arc<AtomicBool>,
) -> Result<(), TtsError> {
    let mut parts: Vec<String> = Vec::with_capacity(cfg.player_command.len());
    let mut piped = false;
    for a in &cfg.player_command {
        if a.contains("{file}") {
            piped = true;
            parts.push(a.replace("{file}", "/dev/stdin"));
        } else {
            parts.push(a.clone());
        }
    }
    let mut command = Command::new(&parts[0]);
    command
        .args(&parts[1..])
        .stdin(if piped { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = unsafe {
        // SAFETY: grupo próprio para interrupção imediata do reprodutor.
        command.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        })
    }
    .spawn()
    .map_err(|e| TtsError(format!("falha ao iniciar o reprodutor: {e}")))?;
    let pgid = child.id() as i32;

    if piped {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&fragment.bytes);
        }
    }
    let min_wait = Duration::from_secs(fragment.bytes.len() as u64 / 162_000 + 1);
    let deadline = Instant::now() + Duration::from_secs(cfg.timeout_secs).max(min_wait * 4);
    loop {
        if cancel.load(Ordering::SeqCst) {
            kill_group(pgid);
            let _ = child.wait();
            return Err(TtsError("interrompido".into()));
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => return Err(TtsError("reprodutor terminou com erro".into())),
            Ok(None) => {}
            Err(e) => return Err(TtsError(format!("espera do reprodutor falhou: {e}"))),
        }
        if Instant::now() >= deadline {
            kill_group(pgid);
            let _ = child.wait();
            return Err(TtsError(format!(
                "reprodução passou de {}s e foi encerrada",
                cfg.timeout_secs
            )));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn kill_group(pgid: i32) {
    unsafe {
        // SAFETY: sinal para o grupo criado com setpgid(0,0) no filho.
        libc::kill(-pgid, libc::SIGTERM);
    }
    std::thread::sleep(Duration::from_millis(50));
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frases_quebradas_por_ponto() {
        let out = prepare_for_speech("Bom dia. Tudo bem? sim!");
        assert_eq!(out, vec!["Bom dia.", "Tudo bem?", "sim!"]);
    }

    #[test]
    fn código_e_tabelas_removidos() {
        let text =
            "Veja:\n```rust\nfn main() {}\n```\n| a | b |\n|---|---|\n| 1 | 2 |\nDepois disso.";
        let out = prepare_for_speech(text);
        assert!(!out.iter().any(|s| s.contains("fn main")));
        assert!(!out.iter().any(|s| s.contains('|')));
        assert!(out.iter().any(|s| s.contains("Depois disso")));
    }

    #[test]
    fn urls_e_caminhos_removidos() {
        let out =
            prepare_for_speech("Veja https://example.com/a/b e /usr/local/bin/coisa e fo_a_zinho.");
        let joined = out.join(" ");
        assert!(!joined.contains("https"));
        assert!(!joined.contains("/usr/local"));
        assert!(joined.contains("fo_a_zinho"));
    }

    #[test]
    fn markdown_leve_removido() {
        let out = prepare_for_speech("**Negrito** e *.cursivo*\n# Título");
        let joined = out.join(" ");
        assert!(!joined.contains("**"));
        assert!(!joined.contains('#'));
    }

    #[test]
    fn inline_code_mantém_texto() {
        let out = prepare_for_speech("Use `restart_zc` agora.");
        let joined = out.join(" ");
        assert!(joined.contains("restart_zc"));
        assert!(!joined.contains('`'));
    }

    #[test]
    fn texto_sem_faláveis_nada() {
        assert!(prepare_for_speech("```rust\nfn a() {}\n```").is_empty());
    }

    fn make_wav(rate: u32, channels: u16) -> Vec<u8> {
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
    fn playback_da_voz_22050_hz() {
        // prova do relato do utiliário: a voz produz 22050 Hz mono
        let info = parse_wav(&make_wav(22_050, 1), u64::MAX as usize).unwrap();
        assert_eq!(info.sample_rate, 22_050);
        info.check_playback().unwrap();
    }
}
