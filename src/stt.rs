//! Transcrição local (Whisper): roda o comando configurado sobre o WAV
//! gravado, com prazo, teto de saída e descarte de marcadores de silêncio.

use std::fmt;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct SttError(String);

impl fmt::Display for SttError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SttError {}

pub struct WhisperConfig {
    pub command: Vec<String>,
    pub model: Option<String>,
    pub language: String,
    pub timeout_secs: u64,
    pub max_chars: usize,
    pub silence_markers: Vec<String>,
}

pub fn transcribe(cfg: &WhisperConfig, audio: &Path) -> Result<String, SttError> {
    if !audio.exists() {
        return Err(SttError(format!(
            "arquivo de áudio {} não existe",
            audio.display()
        )));
    }
    let mut parts: Vec<String> = Vec::with_capacity(cfg.command.len() + 1);
    for a in &cfg.command {
        let s = a
            .replace("{audio}", &audio.display().to_string())
            .replace("{lang}", &cfg.language)
            .replace("{model}", cfg.model.as_deref().unwrap_or(""));
        parts.push(s);
    }
    if parts[0].trim().is_empty() {
        return Err(SttError("comando do Whisper vazio".into()));
    }
    let deadline = Instant::now() + Duration::from_secs(cfg.timeout_secs);
    let mut child = unsafe {
        // SAFETY: grupo próprio para podermos encerrar o comando no prazo.
        Command::new(&parts[0])
            .args(&parts[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            })
    }
    .spawn()
    .map_err(|e| SttError(format!("falha ao iniciar Whisper: {e}")))?;
    let pgid = child.id() as i32;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| SttError("sem saída padrão do Whisper".into()))?;
    let mut text = String::new();
    let mut bytes = 0usize;
    let mut buf = [0u8; 4096];
    let mut timed_out = false;
    loop {
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                bytes += n;
                if bytes > 2 * 1024 * 1024 {
                    break; // teto de saída
                }
                match std::str::from_utf8(&buf[..n]) {
                    Ok(s) => text.push_str(s),
                    Err(_) => text.push('\u{fffd}'),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                kill_group(pgid);
                let _ = child.wait();
                return Err(SttError(format!("leitura da saída do Whisper falhou: {e}")));
            }
        }
    }
    kill_group(pgid);
    let status = child.wait();
    if timed_out {
        return Err(SttError(format!(
            "transcrição passou do prazo de {}s",
            cfg.timeout_secs
        )));
    }
    if let Err(e) = status {
        return Err(SttError(format!("espera do Whisper falhou: {e}")));
    }
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        return Err(SttError("Whisper não produziu texto".into()));
    }
    let capped = crate::sanitize::cap_chars(&trimmed, cfg.max_chars);
    Ok(capped)
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

/// Descarta carimbos de tempo do Whisper — `[00:00:01.250 --> 00:00:17.480]`,
/// com ou sem milissegundos — que alguns builds imprimem mesmo com
/// `--no-prints`. Ao modelo só pode chegar o que foi DITO: a duração da
/// gravação é ruído verboso que ia parar dentro do prompt. É carimbo quando
/// o conteúdo do par de colchetes é puro relógio (dígitos, `:`, `.`, `-->`,
/// espaço) e contém `-->`; colchete de texto de verdade é preservado.
fn remove_carimbos_de_tempo(texto: &str) -> String {
    let chars: Vec<char> = texto.chars().collect();
    let mut saida = String::with_capacity(texto.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some(fim_rel) = chars[i..].iter().take(64).position(|&c| c == ']') {
                let interno: String = chars[i + 1..i + fim_rel].iter().collect();
                let e_carimbo = !interno.is_empty()
                    && interno.contains("-->")
                    && interno
                        .chars()
                        .all(|c| c.is_ascii_digit() || matches!(c, ':' | '.' | '-' | '>' | ' '));
                if e_carimbo {
                    i += fim_rel + 1;
                    continue;
                }
            }
        }
        saida.push(chars[i]);
        i += 1;
    }
    saida
}

/// Remove carimbos de tempo, marcadores de silêncio e espaços; vazio vira
/// `None`. Ponto ÚNICO de limpeza da transcrição (local e servidor): tudo
/// que chega ao modelo passa por aqui.
pub fn strip_silence_markers(text: &str, markers: &[String]) -> Option<String> {
    let mut t = remove_carimbos_de_tempo(text.trim());
    for m in markers {
        while t.contains(m.as_str()) {
            t = t.replace(m.as_str(), " ");
        }
    }
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Endpoint de servidor local, se configurado: envia o WAV e espera texto.
/// VERIFY: a API exata depende da instalação do whisper-server do usuário.
pub fn transcribe_server(url: &str, audio: &Path, timeout_secs: u64) -> Result<String, SttError> {
    if !url.starts_with("http://127.0.0.1") && !url.starts_with("http://localhost") {
        return Err(SttError(
            "whisper.server_url precisa ser um endereço de loopback".into(),
        ));
    }
    let audio_bytes =
        std::fs::read(audio).map_err(|e| SttError(format!("falha ao ler áudio: {e}")))?;
    let boundary = "sexta-feira-wav";
    let mut body: Vec<u8> = Vec::new();
    let header = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
    );
    body.extend_from_slice(header.as_bytes());
    body.extend_from_slice(&audio_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = format!(
        "POST /transcribe HTTP/1.1\r\nHost: {}\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        url.trim_start_matches("http://"),
        body.len()
    );
    let host = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("127.0.0.1");
    let mut stream = connect_loopback(host, timeout_secs)?;
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(&body))
        .map_err(|e| SttError(format!("falha ao enviar áudio ao servidor: {e}")))?;
    let mut response = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut s = stream;
    loop {
        if Instant::now() >= deadline {
            return Err(SttError(
                "servidor de transcrição não respondeu a tempo".into(),
            ));
        }
        let mut buf = [0u8; 4096];
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(SttError(format!("leitura do servidor falhou: {e}"))),
        }
    }
    let text = String::from_utf8_lossy(&response);
    let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    Ok(text[body_start..].trim().to_string())
}

fn connect_loopback(
    host_and_port: &str,
    timeout_secs: u64,
) -> Result<std::net::TcpStream, SttError> {
    use std::net::ToSocketAddrs;
    let addr = host_and_port
        .to_socket_addrs()
        .map_err(|e| SttError(format!("endereço inválido: {e}")))?
        .find(|a| a.ip().is_loopback())
        .ok_or_else(|| SttError("endereço não é loopback".into()))?;
    let stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(timeout_secs))
        .map_err(|e| SttError(format!("conexão ao servidor local falhou: {e}")))?;
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok();
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_wav16k() -> Vec<u8> {
        let data_len = 2usize;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&16_000u32.to_le_bytes());
        out.extend_from_slice(&32_000u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data_len as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 2]);
        out
    }

    fn write_fixture(dir: &Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, make_wav16k()).unwrap();
        p
    }

    #[test]
    fn carimbo_de_tempo_do_whisper_nao_vira_texto() {
        // O bug real: o modelo recebia a DURAÇÃO da gravação junto com a fala.
        assert_eq!(
            strip_silence_markers(
                "[00:00:00.000 --> 00:00:17.000] leia o arquivo de trabalho",
                &[]
            ),
            Some("leia o arquivo de trabalho".into())
        );
    }

    #[test]
    fn carimbos_no_meio_da_frase_sao_removidos() {
        assert_eq!(
            strip_silence_markers(
                "início [00:00:01 --> 00:00:02] do meio [00:00:03.500 --> 00:00:04.250] fim",
                &[]
            ),
            Some("início do meio fim".into())
        );
    }

    #[test]
    fn colchete_de_texto_de_verdade_e_preservado() {
        assert_eq!(
            strip_silence_markers("veja a seção [nota] do documento", &[]),
            Some("veja a seção [nota] do documento".into())
        );
    }

    #[test]
    fn so_carimbo_e_silencio_vira_none() {
        assert_eq!(
            strip_silence_markers("[00:00:00.000 --> 00:00:09.000]", &[]),
            None
        );
    }

    #[test]
    fn marcadores_de_silencio_removidos() {
        assert_eq!(
            strip_silence_markers("[BLANK_AUDIO]  bom dia ", &["[BLANK_AUDIO]".into()]),
            Some("bom dia".into())
        );
        assert_eq!(
            strip_silence_markers("   [SILENCE] ", &["[SILENCE]".into()]),
            None
        );
    }

    #[test]
    fn transcrição_por_comando_falso_fluxo_completo() {
        let tmp = tempfile::tempdir().unwrap();
        let audio = write_fixture(tmp.path(), "a.wav");
        let script =
            "echo '[BLANK_AUDIO] caf\u{e9} da manh\u{e3}'; exec 1>&-; sleep 30".to_string();
        let cfg = WhisperConfig {
            command: vec!["/bin/sh".into(), "-c".into(), script, "{audio}".into()],
            model: None,
            language: "pt".into(),
            timeout_secs: 5,
            max_chars: 100,
            silence_markers: vec!["[BLANK_AUDIO]".into()],
        };
        let text = transcribe(&cfg, &audio).unwrap();
        assert_eq!(
            strip_silence_markers(&text, &cfg.silence_markers),
            Some("caf\u{e9} da manh\u{e3}".into())
        );
    }

    #[test]
    fn arquivo_inexistente_e_erro() {
        let cfg = WhisperConfig {
            command: vec!["/bin/true".into()],
            model: None,
            language: "pt".into(),
            timeout_secs: 1,
            max_chars: 10,
            silence_markers: vec![],
        };
        assert!(transcribe(&cfg, Path::new("/não/existe.wav")).is_err());
    }

    #[test]
    fn servidor_fora_do_loopback_rejeita() {
        let tmp = tempfile::tempdir().unwrap();
        let audio = write_fixture(tmp.path(), "a.wav");
        assert!(transcribe_server("http://10.0.0.1:9000", &audio, 1).is_err());
    }
}
