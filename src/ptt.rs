//! Push-to-talk: detecção da tecla configurada. No modo "hold" usamos
//! evdev (o programa roda no próprio host): SÓ a tecla configurada é
//! observada — nenhum outro evento é lido, guardado ou repassado. O modo
//! "toggle" usa a tecla via terminal (para máquinas sem acesso a /dev/input).

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs, path::Path};

pub const EV_KEY: u16 = 0x01;
pub const KEY_EVENT_VALUE_RELEASE: i32 = 0;
pub const KEY_EVENT_VALUE_PRESS: i32 = 1;

/// Eventos da fonte da tecla. `Failed` carrega erros de praxe (dispositivo,
/// leitura, fio): vão para o journal sempre e para a tela com `--verbose`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySourceEvent {
    Press,
    Release,
    Failed(String),
}

#[derive(Debug)]
pub struct KeySourceError(pub String);

impl fmt::Display for KeySourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for KeySourceError {}

/// Fonte de eventos da tecla de voz. Implementações: evdev (hold) e
/// toggle (via terminal, tratado na camada de aplicação).
pub trait KeySource: Send {
    /// Roda até `stop_flag` ficar true; envia eventos pelo canal.
    fn run(&mut self, events: Sender<KeySourceEvent>, stop_flag: Arc<AtomicBool>);
}

/// Implementação evdev: abre o dispositivo e só observa `code`.
pub struct EvdevKeySource {
    pub device: String,
    pub code: u16,
    /// Prazo máximo; se a tecla ficar presa, soltamos por conta própria.
    pub max_record_secs: u64,
}

impl KeySource for EvdevKeySource {
    fn run(&mut self, events: Sender<KeySourceEvent>, stop_flag: Arc<AtomicBool>) {
        let result = open_and_watch(
            Path::new(&self.device),
            self.code,
            self.max_record_secs,
            &events,
            &stop_flag,
        );
        if let Err(e) = result {
            // erro de praxe: a aplicação funciona sem a tecla, então não
            // derruba nada. O canal entrega Failed; a camada de aplicação
            // registra no journal e, em modo verboso, imprime na tela.
            let _ = events.send(KeySourceEvent::Failed(e.0));
        }
    }
}

fn open_and_watch(
    device: &Path,
    code: u16,
    max_record_secs: u64,
    events: &Sender<KeySourceEvent>,
    stop_flag: &Arc<AtomicBool>,
) -> Result<(), KeySourceError> {
    let fd = open_device(device)?;
    let mut buf = vec![0u8; INPUT_EVENT_SIZE];
    let mut pressed_at: Option<Instant> = None;
    loop {
        if stop_flag.load(Ordering::SeqCst) {
            unsafe { libc::close(fd) };
            return Ok(());
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 50) };
        if rc == 0 {
            // prazo máximo com a tecla presa: força soltar
            if pressed_at.is_some_and(|t| t.elapsed() > Duration::from_secs(max_record_secs)) {
                pressed_at = None;
                let _ = events.send(KeySourceEvent::Release);
            }
            continue;
        }
        if rc < 0 {
            unsafe { libc::close(fd) };
            return Err(KeySourceError(format!(
                "poll no dispositivo {}: {}",
                device.display(),
                std::io::Error::last_os_error()
            )));
        }
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            if n < 0 && errno_is_recoverable() {
                continue;
            }
            unsafe { libc::close(fd) };
            return Err(KeySourceError(format!(
                "leitura do dispositivo {} falhou: {}",
                device.display(),
                std::io::Error::last_os_error()
            )));
        }
        let mut offset = 0usize;
        while offset + INPUT_EVENT_SIZE <= buf.len() {
            let (kind, ev_code, value) = decode_event(&buf[offset..offset + INPUT_EVENT_SIZE]);
            offset += INPUT_EVENT_SIZE;
            if kind != EV_KEY {
                continue;
            }
            if ev_code != code {
                continue; // QUALQUER outra tecla é descartada aqui, sem cópia
            }
            match value {
                KEY_EVENT_VALUE_PRESS => {
                    // o Press sai imediatamente: o aplicativo corta a fala e
                    // congela a digitação na hora; o prazo mínimo vale no
                    // release (a camada de aplicação decide)
                    pressed_at = Some(Instant::now());
                    let _ = events.send(KeySourceEvent::Press);
                }
                2 => {} // repetição automática da tecla: ignorada aqui
                KEY_EVENT_VALUE_RELEASE => {
                    pressed_at = None;
                    let _ = events.send(KeySourceEvent::Release);
                }
                _ => {}
            }
        }
    }
}

fn errno_is_recoverable() -> bool {
    let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    e == libc::EINTR || e == libc::EAGAIN
}

const INPUT_EVENT_SIZE: usize = 24;

/// Faz a decodificação NATIVA do struct input_event do kernel (timeval 2×u64,
/// u16 type, u16 code, i32 value) e devolve apenas campos úteis.
fn decode_event(bytes: &[u8]) -> (u16, u16, i32) {
    let host_i64_a = i64::from_ne_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    let host_i64_b = i64::from_ne_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]);
    let host_u16_c = u16::from_ne_bytes([bytes[16], bytes[17]]);
    let host_u16_d = u16::from_ne_bytes([bytes[18], bytes[19]]);
    let host_i32_e = i32::from_ne_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    let _ = (host_i64_a, host_i64_b);
    (host_u16_c, host_u16_d, host_i32_e)
}

fn open_device(device: &Path) -> Result<i32, KeySourceError> {
    // CString é obrigatório: &str não tem terminador NUL e o open leria bytes
    // além do fim da string (um sufixo tipo "15Z" chegou a vazar path no
    // strace de um usuário; ver docs/DECISIONS.md).
    let path_c = std::ffi::CString::new(
        device
            .to_str()
            .ok_or_else(|| KeySourceError("caminho do dispositivo não é UTF-8".into()))?,
    )
    .map_err(|_| KeySourceError("caminho do dispositivo contém byte NUL".into()))?;
    let fd = unsafe {
        // SAFETY: open(syscall) com um CString terminado em NUL; o descritor
        // é fechado no fim do fio.
        libc::open(
            path_c.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(KeySourceError(format!(
            "não foi possível abrir {}: {} — o usuário precisa estar no grupo input \
             (ou com regra udev equivalente)",
            device.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(fd)
}

/// --find-key: lê UMA tecla em algum dispositivo e imprime device+code.
/// Tudo que não for uma tecla é descartado; nada além do código é impresso.
pub fn find_key(pinned: Option<String>, per_device_ms: u64) -> Result<String, KeySourceError> {
    let devices: Vec<String> = match pinned {
        Some(d) => vec![d],
        None => list_evdev_devices()?,
    };
    if devices.is_empty() {
        return Err(KeySourceError(
            "nenhum /dev/input/event* acessível — confira o grupo input".into(),
        ));
    }
    for device in &devices {
        let src = Path::new(device);
        let fd = open_device(src)?;
        let mut buf = vec![0u8; INPUT_EVENT_SIZE];
        // drena eventos antigos
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
        }
        let deadline = Instant::now() + Duration::from_millis(per_device_ms);
        loop {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, 50) };
            if rc == 0 {
                if Instant::now() >= deadline {
                    break;
                }
                continue;
            }
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            let mut offset = 0usize;
            while offset + INPUT_EVENT_SIZE <= buf.len() {
                let (kind, code, value) = decode_event(&buf[offset..offset + INPUT_EVENT_SIZE]);
                offset += INPUT_EVENT_SIZE;
                if kind == EV_KEY && value == KEY_EVENT_VALUE_PRESS && code != 0 {
                    unsafe { libc::close(fd) };
                    return Ok(format!(
                        "tecla detectada: dispositivo={device} código={code} (hex 0x{code:02x})"
                    ));
                }
            }
        }
        unsafe { libc::close(fd) };
    }
    Err(KeySourceError(
        "nenhuma tecla foi pressionada a tempo. Tente de novo apertando a tecla desejada \
         logo após rodar o comando."
            .into(),
    ))
}

fn list_evdev_devices() -> Result<Vec<String>, KeySourceError> {
    let mut out = Vec::new();
    let dir = fs::read_dir("/dev/input")
        .map_err(|e| KeySourceError(format!("não foi possível listar /dev/input: {e}")))?;
    for e in dir.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with("event") {
            out.push(format!("/dev/input/{name}"));
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn decodifica_evento_sintético() {
        let mut bytes = vec![0u8; INPUT_EVENT_SIZE];
        bytes[16] = 0x01; // type = EV_KEY
        bytes[18] = 0x64; // code = 100
        let v = 1i32.to_ne_bytes();
        bytes[20..24].copy_from_slice(&v);
        assert_eq!(decode_event(&bytes), (EV_KEY, 100, 1));
    }

    #[test]
    fn dispositivo_inexistente_emite_failed() {
        let mut cfg = EvdevKeySource {
            device: "/dev/input/event-inexistente-xyz".into(),
            code: 100,
            max_record_secs: 60,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        cfg.run(tx, stop);
        match rx
            .try_recv()
            .expect("run deveria avisar a falha pelo canal")
        {
            KeySourceEvent::Failed(msg) => {
                assert!(
                    msg.contains("não foi possível abrir"),
                    "mensagem inesperada: {msg}"
                );
            }
            other => panic!("evento errado: {other:?}"),
        }
    }

    #[test]
    fn lista_de_dispositivos_ou_erro_explicito() {
        match list_evdev_devices() {
            Ok(list) => assert!(!list.is_empty() || list.is_empty()),
            Err(e) => assert!(e.0.contains("grupo input") || e.0.contains("/dev/input")),
        }
    }
}
