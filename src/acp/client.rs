//! Cliente ACP sobre WebSocket direto no endpoint `/acp` do gateway do
//! daemon: um fio de leitura classifica as mensagens e entrega em um canal;
//! escritas serializadas; o close é sinalizado e a conexão morre com o
//! cliente. Sem processos filhos e sem ponte.

use std::collections::VecDeque;
use std::io::Write;
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::acp::types::Inbound;
use crate::acp::types::{notification, request};
use crate::acp::ws::{self, WsIn};

#[derive(Debug, Clone)]
pub struct AcpClientError(pub String);

impl std::fmt::Display for AcpClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AcpClientError {}

enum OutMsg {
    Line(String),
    Pong(Vec<u8>),
    Kill,
}

pub struct AcpClient {
    inbox: Receiver<Inbound>,
    backlog: VecDeque<Inbound>,
    out_tx: Mutex<Sender<OutMsg>>,
    closed_flag: Arc<AtomicBool>,
    dead_flag: Arc<AtomicBool>,
    next_id: u64,
}

impl AcpClient {
    /// Conecta ao gateway do daemon (`ws://127.0.0.1:<porta>/acp`).
    /// `token`, quando presente, vai no cabeçalho `Authorization: Bearer`.
    pub fn connect(
        url: &str,
        token: Option<&str>,
        handshake_timeout_secs: u64,
    ) -> Result<AcpClient, AcpClientError> {
        let parsed = ws::parse_ws_url(url).map_err(|e| AcpClientError(e.0))?;
        let timeout = Duration::from_secs(handshake_timeout_secs.max(1));
        let conn = ws::connect(&parsed, token, timeout).map_err(|e| AcpClientError(e.0))?;

        let (tx, rx) = std::sync::mpsc::channel::<Inbound>();
        let closed = Arc::new(AtomicBool::new(false));
        let closed_reader = closed.clone();
        let dead = Arc::new(AtomicBool::new(false));
        let dead_reader = dead.clone();

        let (out_tx, out_rx) = std::sync::mpsc::channel::<OutMsg>();
        let out_reader = out_tx.clone();
        thread::Builder::new()
            .name("acp-reader".into())
            .spawn(move || {
                let mut reader = ws::FrameReader::new(conn.reader);
                'outer: loop {
                    if dead_reader.load(Ordering::SeqCst) {
                        break;
                    }
                    let event = match reader.next_event() {
                        Ok(ev) => ev,
                        Err(e) if e.0 == ws::FrameReader::TIMEOUT_SENTINEL => {
                            continue;
                        }
                        Err(e) => {
                            let _ = tx.send(Inbound::Closed(e.0));
                            break;
                        }
                    };
                    let ws_in = event;
                    match ws_in {
                        WsIn::Text(text) => {
                            for line in text.split('\n') {
                                let line = line.trim();
                                if line.is_empty() {
                                    continue;
                                }
                                match parse_line(line) {
                                    Some(inbound) => {
                                        if tx.send(inbound).is_err() {
                                            break 'outer;
                                        }
                                    }
                                    None => {
                                        let _ = tx.send(Inbound::Closed(format!(
                                            "linha recebida não é um quadro JSON-RPC válido: {}",
                                            limit(line, 120)
                                        )));
                                        break 'outer;
                                    }
                                }
                            }
                        }
                        WsIn::Ping(payload) => {
                            let _ = out_reader.send(OutMsg::Pong(payload));
                        }
                        WsIn::Pong(_) => {}
                        WsIn::Closed => break,
                    }
                }
                closed_reader.store(true, Ordering::SeqCst);
            })
            .map_err(|e| AcpClientError(format!("falha ao iniciar fio leitor: {e}")))?;

        thread::Builder::new()
            .name("acp-writer".into())
            .spawn(move || {
                let mut writer = conn.writer;
                while let Ok(msg) = out_rx.recv() {
                    match msg {
                        OutMsg::Line(line) => {
                            if ws::send_text(&mut writer, &line).is_err() {
                                break;
                            }
                        }
                        OutMsg::Pong(payload) => {
                            if ws::send_pong(&mut writer, &payload).is_err() {
                                break;
                            }
                        }
                        OutMsg::Kill => {
                            ws::send_close(&mut writer);
                            let _ = writer.flush();
                            ws::shutdown(Shutdown::Both, &mut writer);
                            break;
                        }
                    }
                }
            })
            .map_err(|e| AcpClientError(format!("falha ao iniciar fio escritor: {e}")))?;

        Ok(AcpClient {
            inbox: rx,
            backlog: VecDeque::new(),
            out_tx: Mutex::new(out_tx),
            closed_flag: closed,
            dead_flag: dead,
            next_id: 1,
        })
    }

    pub fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn send(&self, line: String) -> Result<(), AcpClientError> {
        self.out_tx
            .lock()
            .expect("mutex de escrita envenenado")
            .send(OutMsg::Line(line))
            .map_err(|_| AcpClientError("fluxo de escrita encerrado".into()))
    }

    pub fn send_request(&mut self, method: &str, params: Value) -> Result<u64, AcpClientError> {
        let id = self.next_id();
        self.send(request(id, method, params).to_string())?;
        Ok(id)
    }

    pub fn send_notification(&self, method: &str, params: Value) -> Result<(), AcpClientError> {
        let line = notification(method, params).to_string();
        self.send(line)
    }

    /// Responde a uma requisição agente → cliente (por exemplo, permissões).
    pub fn reply_request(&self, id: &Value, result: Value) -> Result<(), AcpClientError> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        self.send(payload.to_string())
    }

    /// Responde a uma requisição agente → cliente com um erro JSON-RPC.
    pub fn reply_error(&self, id: &Value, code: i64, message: &str) -> Result<(), AcpClientError> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        });
        self.send(payload.to_string())
    }

    /// Coleta a próxima mensagem, sem bloquear além de `timeout`.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Option<Inbound> {
        if let Some(msg) = self.backlog.pop_front() {
            return Some(msg);
        }
        match self.inbox.recv_timeout(timeout) {
            Ok(msg) => Some(msg),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                Some(Inbound::Closed("fio leitor encerrado".into()))
            }
        }
    }

    /// Devolve mensagens ainda não processadas quando o chamador quer
    /// reordenar (usado por testes).
    pub fn push_back(&mut self, msg: Inbound) {
        self.backlog.push_front(msg);
    }

    pub fn is_closed(&self) -> bool {
        self.closed_flag.load(Ordering::SeqCst)
    }

    /// Fecha a conexão com o gateway.
    pub fn terminate(&mut self) {
        self.dead_flag.store(true, Ordering::SeqCst);
        let _ = self
            .out_tx
            .lock()
            .expect("mutex de escrita")
            .send(OutMsg::Kill);
    }
}

fn limit(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

/// Acesso interno apenas para os testes que precisam injetar mensagens.
pub fn parse_line(line: &str) -> Option<Inbound> {
    let root: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return None,
    };
    crate::acp::types::parse_inbound(&root).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_linha_basica() {
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let inbound = parse_line(line).expect("deve parsear");
        match inbound {
            Inbound::Response { id, .. } => assert_eq!(id, 1),
            other => panic!("tipo errado: {other:?}"),
        }
    }

    #[test]
    fn parse_linha_lixo() {
        assert!(parse_line("não é json").is_none());
        let line = json!({"bem":"estranho"}).to_string();
        assert!(parse_line(&line).is_none());
    }
}
