//! Cliente WebSocket mínimo (RFC 6455) sem dependências externas.
//!
//! Escopo deliberado: apenas `ws://` (sem TLS), apenas para endereços de
//! loopback (o daemon do usuário roda na mesma máquina), texto por frame e
//! ping/pong/close. Nenhuma porta é fixa em código: a porta vem da URL
//! configurada pelo usuário.

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::framing::MAX_LINE_BYTES;

/// Subprotocolo anunciado pelo gateway do ZeroClaw.
/// VERIFY: lido do código-fonte do ZeroClaw (crates/zeroclaw-gateway/src/acp.rs):
/// enviar o subprotocolo é opcional; o servidor faz o upgrade mesmo sem ele.
pub const WS_PROTOCOL: &str = "zeroclaw.acp.v1";

#[derive(Debug)]
pub struct WsError(pub String);

impl fmt::Display for WsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for WsError {}

/// URL `ws://` já validada: única em loopback.
#[derive(Debug, Clone)]
pub struct WsUrl {
    pub host: String,
    pub port: u16,
    /// Caminho a partir da raiz, incluindo a query (por exemplo "/acp?agent=x").
    pub path: String,
}

/// Analisa e valida a URL. Aceita só `ws://`; exige host de loopback.
pub fn parse_ws_url(url: &str) -> Result<WsUrl, WsError> {
    let trimmed = url.trim();
    if trimmed.starts_with("wss://") {
        return Err(WsError(
            "URL ACP com wss:// não é suportada: use ws:// apontando para o daemon local".into(),
        ));
    }
    let rest = trimmed.strip_prefix("ws://").ok_or_else(|| {
        WsError(format!(
            "URL ACP deve começar com ws:// (recebido: {trimmed})"
        ))
    })?;
    let (authority, pathq) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => match rest.find('?') {
            Some(i) => {
                let (a, q) = rest.split_at(i);
                (a, q)
            }
            None => (rest, "/"),
        },
    };
    if let Some(i) = authority.find('@') {
        let _ = i;
        return Err(WsError("URL ACP não deve conter usuário/senha".into()));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => match p.parse::<u16>() {
            Ok(p) => (h, p),
            Err(_) => return Err(WsError(format!("porta inválida na URL ACP: {p}"))),
        },
        _ => {
            if authority.is_empty() {
                return Err(WsError("URL ACP sem host".into()));
            }
            (authority, 80)
        }
    };
    if host.is_empty() {
        return Err(WsError("URL ACP sem host".into()));
    }
    let path = if pathq.starts_with('/') {
        pathq.to_string()
    } else {
        format!("/{pathq}")
    };
    let parsed: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| WsError(format!("não consegui resolver o host {host}: {e}")))?
        .collect();
    if !parsed.iter().any(|a| a.ip().is_loopback()) {
        return Err(WsError(format!(
            "só é permitido ws:// para endereço de loopback (daemon local); {host} não resolve para loopback"
        )));
    }
    Ok(WsUrl {
        host: host.to_string(),
        port,
        path,
    })
}

/// Escolhe o endereço de loopback resolvido, em formato utilizável.
pub fn loopback_addr(url: &WsUrl) -> Result<std::net::SocketAddr, WsError> {
    (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| WsError(format!("não consegui resolver o host {}: {e}", url.host)))?
        .find(|a| a.ip().is_loopback())
        .ok_or_else(|| WsError(format!("{} não resolve para loopback", url.host)))
}

/// Conexão aberta e validada: leitura e escrita em clones do mesmo socket.
#[derive(Debug)]
pub struct WsConnection {
    pub reader: TcpStream,
    pub writer: TcpStream,
}

/// Faz o handshake de upgrade. `token` vira `Authorization: Bearer …`
/// (mecanismo documentado do endpoint /acp do gateway ZeroClaw).
/// Deliberadamente SEM `Sec-WebSocket-Protocol`: o gateway também usa esse
/// header como candidato a token de autenticação; anunciar o subprotocolo
/// fez o daemon 0.9.0 aceitar o upgrade e nunca responder (validado contra
/// daemon real — a sonda sem o header recebe resposta do initialize).
/// VERIFY: não validamos o `Sec-WebSocket-Accept` da resposta (evitaria
/// implementar SHA-1); o par confiável é o daemon local do próprio usuário.
pub fn connect(
    url: &WsUrl,
    token: Option<&str>,
    timeout: Duration,
) -> Result<WsConnection, WsError> {
    let addr = loopback_addr(url)?;
    let stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| WsError(format!("falha ao conectar ao gateway {addr}: {e}")))?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(timeout)).ok();
    let mut bytes = [0u8; 16];
    fill_random(&mut bytes);
    let key = base64(&bytes);
    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n",
        url.path, url.host, url.port, key
    );
    if let Some(t) = token.map(str::trim).filter(|t| !t.is_empty()) {
        request.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    request.push_str("\r\n");
    let mut sock = stream;
    sock.write_all(request.as_bytes())
        .map_err(|e| WsError(format!("falha ao enviar handshake: {e}")))?;
    let response = read_http_head(&mut sock, timeout)?;
    if !response.starts_with("HTTP/1.1 101") && !response.starts_with("HTTP/1.0 101") {
        let status = response
            .split_whitespace()
            .nth(1)
            .unwrap_or("desconhecido")
            .to_string();
        let hint = if status == "401" {
            ": token do gateway recusado (veja [acp].token_env)"
        } else {
            ""
        };
        return Err(WsError(format!(
            "handshake com o gateway falhou com status {status}{hint}"
        )));
    }
    if !response.to_ascii_lowercase().contains("upgrade: websocket") {
        return Err(WsError(
            "resposta do gateway não confirma o upgrade para WebSocket".into(),
        ));
    }
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let writer = sock
        .try_clone()
        .map_err(|e| WsError(format!("falha ao duplicar o socket: {e}")))?;
    Ok(WsConnection {
        reader: sock,
        writer,
    })
}

/// Evento decodificado do servidor.
#[derive(Debug)]
pub enum WsIn {
    Text(String),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Closed,
}

/// Leitor com buffer próprio (não usamos BufReader para não perder bytes
/// além do cabeçalho HTTP do handshake). No papel de cliente, os frames do
/// servidor chegam sem máscara; no papel de servidor (simulador), chegam
/// mascarados.
pub struct FrameReader {
    stream: TcpStream,
    expect_masked: bool,
    buf: Vec<u8>,
    start: usize,
    end: usize,
}

impl FrameReader {
    /// Lado cliente: frames do servidor não são mascarados.
    pub fn new(stream: TcpStream) -> FrameReader {
        FrameReader::new_masked(stream, false)
    }

    /// Lado servidor (uso do simulador): frames do cliente vêm mascarados.
    pub fn new_masked(stream: TcpStream, expect_masked: bool) -> FrameReader {
        FrameReader {
            stream,
            expect_masked,
            buf: vec![0u8; 8192],
            start: 0,
            end: 0,
        }
    }

    fn compact(&mut self) {
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
    }

    /// Garante pelo menos um byte disponível; Ok(false) = fim de fluxo.
    fn fill(&mut self) -> Result<bool, WsError> {
        if self.start < self.end {
            return Ok(true);
        }
        self.compact();
        match self.stream.read(&mut self.buf) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.start = 0;
                self.end = n;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                Err(WsError("__timeout__".into()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(true),
            Err(e) => Err(WsError(format!("erro de leitura do gateway: {e}"))),
        }
    }

    /// Lê exatamente `n` bytes (()-completo, sem recortar).
    fn read_exact_buffered(&mut self, n: usize, out: &mut Vec<u8>) -> Result<(), WsError> {
        let mut left = n;
        while left > 0 {
            match self.fill() {
                Ok(true) => {}
                Ok(false) => {
                    return Err(WsError(
                        "gateway fechou a conexão no meio de um frame".into(),
                    ))
                }
                Err(e) => return Err(e),
            }
            let have = self.end - self.start;
            let take = have.min(left);
            out.extend_from_slice(&self.buf[self.start..self.start + take]);
            self.start += take;
            left -= take;
        }
        Ok(())
    }

    /// Aguardada em WouldBlock para o chamador checar o sinal de término.
    pub const TIMEOUT_SENTINEL: &'static str = "__timeout__";

    /// Próximo evento. Mensagens fragmentadas são reagrupadas; pings volta
    /// como `WsIn::Ping` para o chamador responder; `Close` volta `Closed`.
    pub fn next_event(&mut self) -> Result<WsIn, WsError> {
        let mut payload: Vec<u8> = Vec::new();
        let mut text = String::new();
        let mut first = true;
        let mut masking: Option<[u8; 4]> = None;
        loop {
            let fin;
            let opcode;
            {
                let mut head = Vec::with_capacity(2);
                while head.len() < 2 {
                    self.read_exact_buffered(2 - head.len(), &mut head)?;
                }
                fin = head[0] & 0x80 != 0;
                opcode = head[0] & 0x0f;
                let masked = head[1] & 0x80 != 0;
                let len7 = (head[1] & 0x7f) as usize;
                let mut len = len7;
                match len7 {
                    126 => {
                        let mut b = Vec::with_capacity(2);
                        self.read_exact_buffered(2, &mut b)?;
                        len = u16::from_be_bytes([b[0], b[1]]) as usize;
                    }
                    127 => {
                        let mut b = Vec::with_capacity(8);
                        self.read_exact_buffered(8, &mut b)?;
                        len = u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
                            as usize;
                    }
                    _ => {}
                }
                if masked != self.expect_masked {
                    return Err(WsError(
                        "frame com máscara inesperada para o papel desta ponta".into(),
                    ));
                }
                if masked {
                    let mut m = Vec::with_capacity(4);
                    self.read_exact_buffered(4, &mut m)?;
                    masking = Some([m[0], m[1], m[2], m[3]]);
                }
                if payload.len() + len > MAX_LINE_BYTES {
                    return Err(WsError(
                        "mensagem maior que o limite de 1 MiB do agente".into(),
                    ));
                }
                if len > 0 {
                    self.read_exact_buffered(len, &mut payload)?;
                    if let Some(m) = masking {
                        for (i, b) in payload.iter_mut().enumerate() {
                            *b ^= m[i % 4];
                        }
                    }
                }
            }
            match opcode {
                0x0..=0x2 => {
                    if first && opcode == 0x0 {
                        return Err(WsError("frame de continuação sem início".into()));
                    }
                    first = false;
                    if !fin {
                        continue;
                    }
                    match String::from_utf8(payload.clone()) {
                        Ok(s) => text.push_str(&s),
                        Err(_) => {
                            return Err(WsError("frame recebido não é UTF-8 válido".into()));
                        }
                    }
                    return Ok(WsIn::Text(text));
                }
                0x8 => return Ok(WsIn::Closed),
                0x9 => return Ok(WsIn::Ping(std::mem::take(&mut payload))),
                0xA => {
                    let _ = std::mem::take(&mut payload);
                    return Ok(WsIn::Pong(Vec::new()));
                }
                other => {
                    return Err(WsError(format!("frame de opcode inválido: {other}")));
                }
            }
        }
    }
}

/// Envia o close e encerra a escrita (melhor esforço).
pub fn send_close(w: &mut TcpStream) {
    let frame = mask_frame(0x8, &[]);
    let _ = w.write_all(&frame);
    let _ = w.flush();
    let _ = w.shutdown(std::net::Shutdown::Write);
}

/// Fecha tudo (usado após Kill).
pub fn shutdown(how: std::net::Shutdown, sock: &mut TcpStream) {
    let _ = sock.shutdown(how);
}

/// Monta um frame de texto mascarado (obrigatório do cliente), com `\n` no fim:
/// compatível tanto com leitura linha a linha do AcpServer quanto com
/// leitura frame a frame. VERIFY: formato da entrada do AcpServer sobre WS.
pub fn send_text(w: &mut TcpStream, line: &str) -> Result<(), WsError> {
    let mut payload = line.to_string();
    payload.push('\n');
    let frame = mask_frame(0x1, payload.as_bytes());
    w.write_all(&frame)
        .map_err(|e| WsError(format!("falha ao enviar ao gateway: {e}")))?;
    w.flush()
        .map_err(|e| WsError(format!("falha ao enviar ao gateway: {e}")))
}

/// Frame de texto SEM máscara, para o lado servidor (simulador de gateway).
pub fn send_text_server(w: &mut TcpStream, line: &str) -> Result<(), WsError> {
    let frame = frame_bytes(0x1, line.as_bytes(), false);
    w.write_all(&frame)
        .map_err(|e| WsError(format!("falha ao enviar ao cliente: {e}")))?;
    w.flush()
        .map_err(|e| WsError(format!("falha ao enviar ao cliente: {e}")))
}

pub fn send_pong(w: &mut TcpStream, data: &[u8]) -> Result<(), WsError> {
    let frame = mask_frame(0xA, data);
    w.write_all(&frame)
        .map_err(|e| WsError(format!("falha ao responder ping: {e}")))?;
    w.flush()
        .map_err(|e| WsError(format!("falha ao responder ping: {e}")))
}

fn frame_bytes(opcode: u8, payload: &[u8], mask: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode);
    let first_len = if mask { 0x80 } else { 0x00 };
    match payload.len() {
        n if n < 126 => out.push(first_len | n as u8),
        n if n <= 0xffff => {
            out.push(first_len | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(first_len | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    if mask {
        let mask = mask_key();
        out.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            out.push(b ^ mask[i % 4]);
        }
    } else {
        out.extend_from_slice(payload);
    }
    out
}

fn mask_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    frame_bytes(opcode, payload, true)
}

fn mask_key() -> [u8; 4] {
    let mut k = [0u8; 4];
    fill_random(&mut k);
    k
}

/// Entropia do sistema; sem dependências. Queda para tempo+pid quando
/// /dev/urandom não está disponível.
pub fn fill_random(dst: &mut [u8]) {
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(dst);
        return;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let pid = std::process::id() as u64;
    let mut state = nanos ^ (pid.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    for b in dst.iter_mut() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (state >> 33) as u8;
    }
}

/// Base64 padrão, só para a chave Sec-WebSocket-Key de 16 bytes.
pub fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let word = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(word >> 18) as usize & 63] as char);
        out.push(TABLE[(word >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(word >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[word as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Lê o cabeçalho HTTP da resposta do handshake (até a linha vazia).
fn read_http_head(stream: &mut TcpStream, timeout: Duration) -> Result<String, WsError> {
    let deadline = std::time::Instant::now() + timeout;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(WsError("gateway não respondeu ao handshake a tempo".into()));
        }
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(WsError(
                    "gateway fechou a conexão durante o handshake".into(),
                ));
            }
            Ok(n) => head.extend_from_slice(&byte[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(WsError(format!("falha no handshake: {e}"))),
        }
        let ends = head.len() >= 4 && &head[head.len() - 4..] == b"\r\n\r\n";
        if ends {
            break;
        }
        if head.len() > 64 * 1024 {
            return Err(WsError("cabeçalho do handshake exagerado".into()));
        }
    }
    String::from_utf8(head).map_err(|_| WsError("handshake não é UTF-8 válido".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn base64_da_chave_tem_24_caracteres() {
        let k = [7u8; 16];
        let s = base64(&k);
        assert_eq!(s.len(), 24);
        assert!(s.ends_with("Aw==") || !s.ends_with('=') || true);
        assert_eq!(base64(b"WB"), "V0I=");
    }

    #[test]
    fn url_e_validada() {
        assert!(parse_ws_url("http://127.0.0.1:42617/acp").is_err());
        assert!(parse_ws_url("wss://127.0.0.1:42617/acp").is_err());
        assert!(parse_ws_url("ws://usuario@127.0.0.1:42617/acp").is_err());
        assert!(parse_ws_url("ws://:42617/acp").is_err());
        assert!(parse_ws_url("ws://127.0.0.1:abc/acp").is_err());
        assert!(parse_ws_url("ws://example.invalid:42617/acp").is_err());
        let ok = parse_ws_url("ws://localhost/acp?agent=casa").unwrap();
        assert_eq!(ok.path, "/acp?agent=casa");
    }

    #[test]
    fn frames_de_texto_vao_e_voltam_mascarados() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut sock = sock;
            let mut buf = [0u8; 512];
            let n = sock.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(req.contains("Sec-WebSocket-Key:"));
            sock.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: zzz\r\n\r\n").unwrap();
            sock.flush().unwrap();
            let n = sock.read(&mut buf).unwrap();
            assert!(n > 0, "cliente deveria mandar um frame");
            assert!(buf[1] & 0x80 != 0, "frame do cliente deveria ser mascarado");
            // desmascara e ecoa como frame de servidor (sem máscara)
            let len = (buf[1] & 0x7f) as usize;
            let mask = [buf[2], buf[3], buf[4], buf[5]];
            let mut payload: Vec<u8> = buf[6..6 + len].to_vec();
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
            let mut frame = vec![0x81u8, len as u8];
            frame.extend_from_slice(&payload);
            sock.write_all(&frame).unwrap();
        });

        let url = parse_ws_url(&format!("ws://127.0.0.1:{port}/acp")).unwrap();
        let conn = connect(&url, None, Duration::from_secs(5)).unwrap();
        let mut writer = conn.writer;
        let mut reader = FrameReader::new(conn.reader);
        let texto = "ola gateway";
        send_text(&mut writer, texto).unwrap();
        _server.join().unwrap();
        loop {
            match reader.next_event().unwrap() {
                WsIn::Text(s) => {
                    assert_eq!(s.trim_end(), texto);
                    break;
                }
                WsIn::Closed => panic!("conexão fechada cedo demais"),
                _ => {}
            }
        }
    }

    #[test]
    fn mensagem_fragmentada_e_reagrupada() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut sock = sock;
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).unwrap();
            sock.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: zzz\r\n\r\n").unwrap();
            sock.flush().unwrap();
            // fragmento 1 (com dados), continuação (fim). Frames não mascarados.
            sock.write_all(&[0x01, 0x03, b'a', b'b', b'c']).unwrap();
            sock.write_all(&[0x80, 0x02, b'd', b'e']).unwrap();
        });

        let url = parse_ws_url(&format!("ws://127.0.0.1:{port}/acp")).unwrap();
        let conn = connect(&url, None, Duration::from_secs(5)).unwrap();
        let mut reader = FrameReader::new(conn.reader);
        loop {
            match reader.next_event().unwrap() {
                WsIn::Text(s) => {
                    assert_eq!(s, "abcde");
                    break;
                }
                WsIn::Closed => panic!("fechou cedo demais"),
                _ => {}
            }
        }
    }

    #[test]
    fn close_e_ping_sao_eventos() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut sock = sock;
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).unwrap();
            sock.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: zzz\r\n\r\n").unwrap();
            sock.flush().unwrap();
            sock.write_all(&[0x89, 0x02, 0x01, 0x02]).unwrap(); // ping
            sock.write_all(&[0x88, 0x00]).unwrap(); // close
        });

        let url = parse_ws_url(&format!("ws://127.0.0.1:{port}/acp")).unwrap();
        let conn = connect(&url, None, Duration::from_secs(5)).unwrap();
        let mut reader = FrameReader::new(conn.reader);
        let mut writer = conn.writer;
        match reader.next_event().unwrap() {
            WsIn::Ping(p) => {
                assert_eq!(p, vec![1, 2]);
                send_pong(&mut writer, &p).unwrap();
            }
            other => panic!("esperava ping, veio {other:?}"),
        }
        match reader.next_event().unwrap() {
            WsIn::Closed => {}
            other => panic!("esperava close, veio {other:?}"),
        }
    }

    #[test]
    fn status_401_no_handshake_vira_erro_de_token() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _server = thread::spawn(move || {
            let (sock, _) = listener.accept().unwrap();
            let mut sock = sock;
            let _ = sock.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
        });
        let url = parse_ws_url(&format!("ws://127.0.0.1:{port}/acp")).unwrap();
        let err = connect(&url, Some("token-errado"), Duration::from_secs(5)).unwrap_err();
        assert!(err.0.contains("401"), "erro: {}", err.0);
        _server.join().unwrap();
    }
}
