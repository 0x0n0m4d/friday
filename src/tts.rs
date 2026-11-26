//! Voz: prepara o texto do modelo para fala (filter), enfileira em frases e
//! toca com o Piper nativo + um reprodutor configurado. Falhas nunca são
//! silenciosas: aparecem na barra de status (sem nunca falar o texto).

use std::collections::VecDeque;
use std::fmt;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, SyncSender};
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

#[derive(Clone)]
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

// ---------- fio do falante (pipeline em três partes) ----------

/// Tamanho mínimo (em caracteres) do fragmento que vai ao Piper. Frases
/// curtas (listas, saudações) são unidas ao que vier depois: cada fragmento
/// é um spawn de processo e cada silêncio entre spawns é audível — fragmento
/// miúdo demais soa exatamente como o "trava no meio da frase" relatado.
pub const FRAG_MINIMO_CHARS: usize = 120;

/// Espera ao reprocessar a fila quando o canal de síntese está cheio. Curto
/// de propósito: Interrupt entre um fragmento e outro não pode esperar uma
/// síntese inteira para ser atendido.
const RELOGIO_FILA: Duration = Duration::from_millis(15);

enum SpeakerCommand {
    Speak(String),
    Interrupt,
    Shutdown,
}

/// Tarefa de síntese (texto → WAV). O `epoch` marca a rodada de fala: quando
/// um Interrupt chega, a rodada cresce e tudo que carrega rodada antiga vira
/// lixo imediato — sem flag booleana de cancelamento que precisa ser zerada
/// na mão (a fonte de corridas da versão antiga).
struct SynthTask {
    epoch: u64,
    sentence: String,
}

/// Tarefa de reprodução (WAV → som). Um único fio reproduz, então a ordem do
/// canal é a ordem do som — nenhuma frase fala fora de vez.
struct PlayTask {
    epoch: u64,
    fragment: Fragment,
}

pub struct Speaker {
    tx: Mutex<std::sync::mpsc::Sender<SpeakerCommand>>,
}

impl Speaker {
    /// Dá vida ao pipeline:
    ///
    ///   fila (ordem) ──► piper-synth ──canal(2)──► voz-player
    ///   speak/queue      liga o Piper e     WAV       liga o player e
    ///                    coleta o WAV                 reproduz
    ///
    /// Síntese e reprodução correm em paralelo: enquanto uma frase toca, a
    /// próxima já está sendo sintetizada. Os canais acotados limitam a
    /// antecipação a 2 frases na síntese e 2 áudios na fila de reprodução —
    /// memória teto de 2 áudios, nunca o turno inteiro.
    pub fn new(cfg: SpeakerConfig, events: Sender<TtsEvent>) -> Speaker {
        let (tx, rx) = std::sync::mpsc::channel::<SpeakerCommand>();
        let epoch = Arc::new(AtomicU64::new(0));
        let (synth_tx, synth_rx) = std::sync::mpsc::sync_channel::<SynthTask>(2);
        let (play_tx, play_rx) = std::sync::mpsc::sync_channel::<PlayTask>(2);

        {
            let cfg = cfg.clone();
            let epoch = epoch.clone();
            let events_synth = events.clone();
            let _ = std::thread::Builder::new()
                .name("piper-synth".into())
                .spawn(move || synth_loop(cfg, synth_rx, play_tx, events_synth, epoch));
        }
        {
            let epoch = epoch.clone();
            let events_play = events.clone();
            let _ = std::thread::Builder::new()
                .name("voz-player".into())
                .spawn(move || play_loop(cfg, play_rx, events_play, epoch));
        }
        {
            let epoch = epoch.clone();
            let _ = std::thread::Builder::new()
                .name("speaker".into())
                .spawn(move || speaker_loop(rx, synth_tx, events, epoch));
        }
        Speaker { tx: Mutex::new(tx) }
    }

    /// Enfileira uma frase; a ordem é preservada (uma fila de reprodução só).
    pub fn speak(&self, sentence: String) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(SpeakerCommand::Speak(sentence));
        }
    }

    /// Interrompe imediatamente o que está sendo falado e esvazia a fila.
    /// A rodada cresce: síntese e reprodução descartam tudo que era da
    /// rodada anterior, mesmo com processos ainda em pleno voo.
    pub fn interrupt(&self) {
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

/// Processa comandos pendentes sem bloquear; devolve `false` quando o fio
/// da fila deve sair. Chamado em dois pontos do ciclo — Interrupt nunca fica
/// preso atrás de canal de síntese cheio.
fn comandos_pendentes(
    rx: &std::sync::mpsc::Receiver<SpeakerCommand>,
    queue: &mut VecDeque<String>,
    epoch: &AtomicU64,
    events: &Sender<TtsEvent>,
) -> bool {
    loop {
        match rx.try_recv() {
            Ok(SpeakerCommand::Shutdown) => return false,
            Ok(SpeakerCommand::Interrupt) => {
                epoch.fetch_add(1, Ordering::SeqCst);
                queue.clear();
                let _ = events.send(TtsEvent::Interrupted);
            }
            Ok(SpeakerCommand::Speak(s)) => queue.push_back(s),
            Err(std::sync::mpsc::TryRecvError::Empty) => return true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return false,
        }
    }
}

/// Tira da fila o próximo pedaço de fala, unindo frases curtas até alcançar
/// o tamanho mínimo. Ordem sempre: nada sai da frente de outra frase.
fn proximo_fragmento(queue: &mut VecDeque<String>) -> Option<String> {
    let primeiro = queue.pop_front()?;
    if primeiro.chars().count() >= FRAG_MINIMO_CHARS {
        return Some(primeiro);
    }
    let mut juntado = primeiro;
    while juntado.chars().count() < FRAG_MINIMO_CHARS {
        let Some(proximo) = queue.pop_front() else {
            break;
        };
        juntado.push(' ');
        juntado.push_str(&proximo);
    }
    Some(juntado)
}

/// Fio da fila: recebe Speak/Interrupt/Shutdown e alimenta a síntese. Nunca
/// faz rendição de áudio nem conhece o Piper — só ordena.
fn speaker_loop(
    rx: std::sync::mpsc::Receiver<SpeakerCommand>,
    synth_tx: SyncSender<SynthTask>,
    events: Sender<TtsEvent>,
    epoch: Arc<AtomicU64>,
) {
    let mut queue: VecDeque<String> = VecDeque::new();
    'main: loop {
        match rx.recv() {
            Ok(SpeakerCommand::Shutdown) => break,
            Ok(SpeakerCommand::Interrupt) => {
                epoch.fetch_add(1, Ordering::SeqCst);
                queue.clear();
                let _ = events.send(TtsEvent::Interrupted);
            }
            Ok(SpeakerCommand::Speak(s)) => queue.push_back(s),
            Err(_) => break,
        }
        if !comandos_pendentes(&rx, &mut queue, &epoch, &events) {
            break 'main;
        }
        // Alimenta a síntese com a fila inteira; o canal acotado (2) manda o
        // ritmo e mantém o fio de comandos continuamente atendido.
        while !queue.is_empty() {
            if !comandos_pendentes(&rx, &mut queue, &epoch, &events) {
                break 'main;
            }
            let Some(sentence) = proximo_fragmento(&mut queue) else {
                break;
            };
            let task = SynthTask {
                epoch: epoch.load(Ordering::SeqCst),
                sentence,
            };
            match synth_tx.try_send(task) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(devolvida)) => {
                    queue.push_front(devolvida.sentence);
                    // Síntese adiantada ao máximo: volta ao ciclo de comandos.
                    std::thread::sleep(RELOGIO_FILA);
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break 'main,
            }
        }
    }
}

/// Fio de síntese: um Piper por fragmento, mas SEM esperar ninguém ouvir —
/// ele corre à frente do player. Rodada antiga ou falha: fragmento pulado,
/// pipeline vivo (falha nunca silenciosa: vira evento de erro).
fn synth_loop(
    cfg: SpeakerConfig,
    rx: std::sync::mpsc::Receiver<SynthTask>,
    play_tx: SyncSender<PlayTask>,
    events: Sender<TtsEvent>,
    epoch: Arc<AtomicU64>,
) {
    for task in rx {
        if task.epoch != epoch.load(Ordering::SeqCst) {
            continue; // rodada antiga (Interrupt): nem liga o Piper
        }
        match render_fragment(&cfg, &task.sentence, task.epoch, &epoch) {
            Ok(fragment) => {
                // Bloqueia até o player ter lugar: é a válvula de backpressão.
                // Se a rodada girar, o player descarta o áudio adiante.
                let play = PlayTask {
                    epoch: task.epoch,
                    fragment,
                };
                if play_tx.send(play).is_err() {
                    break; // fio de reprodução morreu: encerra a síntese
                }
            }
            Err(e) => {
                if task.epoch != epoch.load(Ordering::SeqCst) {
                    continue; // interrompido no meio do voo: já foi anunciado
                }
                let _ = events.send(TtsEvent::Finished(Err(e)));
            }
        }
    }
}

/// Fio de reprodução: consome os WAVs prontos em ordem; anuncia Started
/// quando o som de verdade começa, não quando a síntese apenas começou.
fn play_loop(
    cfg: SpeakerConfig,
    rx: std::sync::mpsc::Receiver<PlayTask>,
    events: Sender<TtsEvent>,
    epoch: Arc<AtomicU64>,
) {
    for task in rx {
        let rodada = task.epoch;
        if rodada != epoch.load(Ordering::SeqCst) {
            continue; // rodada antiga: lixo de fila, descarta em silêncio
        }
        let _ = events.send(TtsEvent::Started);
        if rodada != epoch.load(Ordering::SeqCst) {
            continue; // girou no meio do anúncio: não toca nunca
        }
        match play_fragment(&cfg, task.fragment, rodada, &epoch) {
            Ok(()) => {
                if rodada == epoch.load(Ordering::SeqCst) {
                    let _ = events.send(TtsEvent::Finished(Ok(())));
                }
            }
            Err(e) => {
                if rodada == epoch.load(Ordering::SeqCst) {
                    let _ = events.send(TtsEvent::Finished(Err(e)));
                }
                // rodada antiga: Interrupted já foi anunciado pela fila
            }
        }
    }
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
    rodada: u64,
    epoch: &AtomicU64,
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
        let _ = std::thread::Builder::new()
            .name("piper-feed".into())
            .spawn(move || {
                // Escreve de uma vez: se a rodada girou, o grupo morre logo e
                // a escrita falha inofensivamente (pipe quebrado).
                let _ = stdin.write_all(&to_write);
            });
    }

    let result = collect_piper_output(cfg, &mut child, temp_path.as_deref(), rodada, epoch);
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
    rodada: u64,
    epoch: &AtomicU64,
) -> Result<Vec<u8>, TtsError> {
    let deadline = Instant::now() + Duration::from_secs(cfg.timeout_secs);
    if let Some(path) = temp_path {
        loop {
            if rodada != epoch.load(Ordering::SeqCst) {
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
            if rodada != epoch.load(Ordering::SeqCst) {
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
/// reprodutor é substituído por `/dev/stdin`). Um processo por fragmento,
/// mas o pipeline mantém o próximo áudio quase pronto: a troca de fragmento
/// não deixa a voz em silêncio por centenas de milissegundos.
fn play_fragment(
    cfg: &SpeakerConfig,
    fragment: Fragment,
    rodada: u64,
    epoch: &AtomicU64,
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
        if rodada != epoch.load(Ordering::SeqCst) {
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
    fn frases_curtas_sao_unidas_e_a_ordem_e_preservada() {
        // fragmento miúdo demais soa como gagueira: a fila une as curtas
        let mut q: VecDeque<String> = ["a.", "b.", "c."].iter().map(|s| s.to_string()).collect();
        let f = proximo_fragmento(&mut q).unwrap();
        assert_eq!(f, "a. b. c.");
        assert!(q.is_empty());
    }

    #[test]
    fn frase_longa_vai_sozinha_e_a_seguinte_fica_na_fila() {
        let longa = "l".repeat(FRAG_MINIMO_CHARS);
        let mut q: VecDeque<String> = [longa.clone(), "curta.".into()].into();
        assert_eq!(proximo_fragmento(&mut q).unwrap(), longa);
        assert_eq!(proximo_fragmento(&mut q).unwrap(), "curta.");
        assert!(proximo_fragmento(&mut q).is_none());
    }

    #[test]
    fn fila_vazia_nao_tem_fragmento() {
        let mut q: VecDeque<String> = VecDeque::new();
        assert!(proximo_fragmento(&mut q).is_none());
    }

    #[test]
    fn playback_da_voz_22050_hz() {
        // prova do relato do utiliário: a voz produz 22050 Hz mono
        let info = parse_wav(&make_wav(22_050, 1), u64::MAX as usize).unwrap();
        assert_eq!(info.sample_rate, 22_050);
        info.check_playback().unwrap();
    }
}
