//! Agente ACP de brinquedo para os testes e para ensaio manual.
//! Controlado por argumentos de linha de comando (nada disso é interface estável):
//!   --port <n>                   porta de escuta; 0 = escolhe sozinha e imprime "PORT=<n>"
//!   --require-token <token>      exige Authorization: Bearer <token>; responde 401 caso contrário
//!   --busy                       responde session/prompt com -32002
//!   --permission                 pede session/request_permission no turno
//!   --malformed                  envia uma mensagem quebrada após initialize
//!   --load-bad                   session/load e session/resume respondem erro
//!   --load-replay                antes de responder session/load, reenvia o
//!                                histórico salvo como session/update (como
//!                                faz o daemon real na recarga de sessão)
//!   --prompt-unknown-session     o primeiro session/prompt falha como sessão expirada
//!   --prompt-always-unknown-session   TODOS os session/prompt falham como sessão expirada
//!   --exit-after-reply           encerra após a primeira resposta
//!   --no-audio-cap               não anuncia capacidade de áudio
//!   --session-id <id>            id de sessão (padrão "s-mock-1")
//!   --reply <texto>              texto devolvido como agent_message_chunk
//!   --protocol <n>               versão devolvida no initialize (1 padrão)
//!
//! Fala JSON-RPC 2.0 (uma mensagem por frame de texto de WebSocket),
//! imitando o endpoint /acp do gateway do daemon.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sexta_feira::acp::ws::{self, FrameReader, WsIn};

fn main() {
    let mut session_id = "s-mock-1".to_string();
    let mut reply = "Esta é uma resposta de teste do agente de simulação.".to_string();
    let mut busy = false;
    let mut wants_permission = false;
    let mut malformed = false;
    let mut load_ok = true;
    let mut protocol: u64 = 1;
    let mut exit_after_reply = false;
    let mut audio_cap = true;
    let mut prompt_error_first = false;
    let mut prompt_error_always = false;
    let mut load_replay = false;
    let mut port: u16 = 0;
    let mut require_token: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => {
                let v = args.next().unwrap_or_else(|| panic!("--port exige valor"));
                port = v.parse().unwrap_or_else(|_| panic!("--port inválido: {v}"));
            }
            "--require-token" => {
                require_token = Some(
                    args.next()
                        .unwrap_or_else(|| panic!("--require-token exige valor")),
                )
            }
            "--busy" => busy = true,
            "--permission" => wants_permission = true,
            "--malformed" => malformed = true,
            "--load-bad" => load_ok = false,
            "--prompt-unknown-session" => prompt_error_first = true,
            "--prompt-always-unknown-session" => prompt_error_always = true,
            "--load-replay" => load_replay = true,
            "--exit-after-reply" => exit_after_reply = true,
            "--no-audio-cap" => audio_cap = false,
            "--session-id" => {
                session_id = args
                    .next()
                    .unwrap_or_else(|| panic!("--session-id exige valor"))
            }
            "--reply" => reply = args.next().unwrap_or_else(|| panic!("--reply exige valor")),
            "--protocol" => {
                let v = args
                    .next()
                    .unwrap_or_else(|| panic!("--protocol exige valor"));
                protocol = v
                    .parse()
                    .unwrap_or_else(|_| panic!("--protocol inválido: {v}"));
            }
            other => panic!("argumento desconhecido do simulador: {other}"),
        }
    }

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("falha ao ouvir a porta");
    let real_port = listener.local_addr().unwrap().port();
    if port == 0 {
        println!("PORT={real_port}");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }
    for accepted in listener.incoming() {
        let Ok(sock) = accepted else { break };
        let encerrar = serve_connection(
            sock,
            &session_id,
            &reply,
            busy,
            wants_permission,
            malformed,
            load_ok,
            protocol,
            exit_after_reply,
            audio_cap,
            prompt_error_first,
            prompt_error_always,
            load_replay,
            require_token.as_deref(),
        );
        if encerrar {
            break;
        }
    }
}

/// Atende uma conexão; devolve `true` quando o simulador deve encerrar.
#[allow(clippy::too_many_arguments)]
fn serve_connection(
    sock: TcpStream,
    session_id: &str,
    reply: &str,
    busy: bool,
    wants_permission: bool,
    malformed: bool,
    load_ok: bool,
    protocol: u64,
    exit_after_reply: bool,
    audio_cap: bool,
    prompt_error_first: bool,
    prompt_error_always: bool,
    load_replay: bool,
    require_token: Option<&str>,
) -> bool {
    let mut sock = sock;
    let head = match read_request_head(&mut sock, Duration::from_secs(5)) {
        Some(h) => h,
        None => return false,
    };
    if let Some(expected) = require_token {
        let ok = head
            .lines()
            .find_map(|l| {
                if l.len() >= 14 && l[..14].eq_ignore_ascii_case("authorization:") {
                    Some(l["authorization:".len()..].trim().to_string())
                } else {
                    None
                }
            })
            .map(|v| v == format!("Bearer {expected}"))
            .unwrap_or(false);
        if !ok {
            let _ = sock.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
            return false;
        }
    }
    let _ = sock.write_all(
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: simulador\r\n\r\n",
    );
    let _ = sock.flush();

    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let mut reader = match sock.try_clone() {
        Ok(s) => FrameReader::new_masked(s, true),
        Err(_) => return false,
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let prompt_count = Arc::new(AtomicI32::new(0));

    let mut awaiting_permission = false;
    let mut answered = false;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if Instant::now() > deadline {
            return false;
        }
        let event = match reader.next_event() {
            Ok(ev) => ev,
            Err(_) => continue,
        };
        let text = match event {
            WsIn::Text(t) => t,
            WsIn::Ping(p) => {
                let _ = p;
                continue;
            }
            WsIn::Pong(_) | WsIn::Closed => return answered && exit_after_reply,
        };
        for line in text.split('\n') {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let method = msg
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            let has_id = msg.get("id").is_some() && !msg.get("id").unwrap().is_null();
            if awaiting_permission && method.is_empty() {
                // é a resposta do cliente à session/request_permission
                awaiting_permission = false;
            }
            if !has_id {
                if method == "session/cancel" {
                    cancelled.store(true, Ordering::SeqCst);
                }
                continue;
            }
            let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let mut result = serde_json::json!({});
            let mut error: Option<serde_json::Value> = None;
            match method.as_str() {
                "initialize" => {
                    result = serde_json::json!({
                        "protocolVersion": protocol,
                        "capabilities": {
                            "promptCapabilities": {
                                "image": false,
                                "audio": audio_cap,
                                "embeddedContext": true
                            },
                            "loadSession": true,
                            "sessionCapabilities": {"resume": true, "close": true}
                        },
                        "_meta": {"zeroclaw": {
                            "defaultModel": "mock-model",
                            "maxSessions": 10,
                            "sessionTimeoutSecs": 3600
                        }}
                    });
                    if malformed {
                        let _ = ws::send_text_server(&mut sock, "{linha quebrada sem json");
                    }
                }
                "session/new" => {
                    result = serde_json::json!({
                        "sessionId": session_id,
                        "workspaceDir": "/tmp"
                    });
                }
                "session/load" | "session/resume" => {
                    if load_ok {
                        // Recarga realista: o daemon expele o histórico salvo
                        // como session/update ANTES de responder ao load.
                        if load_replay && method == "session/load" {
                            let upd = |kind: &str, text: &str| {
                                serde_json::json!({
                                    "jsonrpc": "2.0", "method": "session/update",
                                    "params": {"sessionId": session_id, "update": {
                                        "sessionUpdate": kind,
                                        "content": {"type": "text", "text": text}
                                    }}
                                })
                            };
                            let _ = ws::send_text_server(
                                &mut sock,
                                &upd(
                                    "user_message_chunk",
                                    "histórico: qual é a capital do Brasil?",
                                )
                                .to_string(),
                            );
                            std::thread::sleep(Duration::from_millis(10));
                            let _ = ws::send_text_server(
                                &mut sock,
                                &upd("agent_message_chunk", "histórico: Brasília.").to_string(),
                            );
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        // Realismo ZeroClaw 0.9.0: session/load responde {}
                        // (SEM sessionId — o id é o que o cliente pediu; ver
                        // docs/DECISIONS.md D11). O cliente usa pending_load_sid.
                        result = serde_json::json!({});
                    } else {
                        error = Some(serde_json::json!({
                            "code": -32000, "message": "sessão desconhecida"
                        }));
                    }
                }
                "session/prompt" => {
                    let nth = prompt_count.fetch_add(1, Ordering::SeqCst) + 1;
                    if prompt_error_always || (prompt_error_first && nth == 1) {
                        error = Some(serde_json::json!({
                            "code": -32000,
                            "message": "sessão desconhecida (expirada)"
                        }));
                    } else if busy {
                        error =
                            Some(serde_json::json!({ "code": -32002, "message": "SESSION_BUSY" }));
                    } else {
                        let mid = reply.len() / 2;
                        let (a, b) = reply.split_at(mid);
                        let upd = |text: &str| {
                            serde_json::json!({
                                "jsonrpc": "2.0", "method": "session/update",
                                "params": {"sessionId": session_id, "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": {"type": "text", "text": text}
                                }}
                            })
                        };
                        let _ = ws::send_text_server(&mut sock, &upd(a).to_string());
                        std::thread::sleep(Duration::from_millis(20));
                        let _ = ws::send_text_server(&mut sock, &upd(b).to_string());
                        if wants_permission {
                            awaiting_permission = true;
                            let req = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": 900,
                                "method": "session/request_permission",
                                "params": {
                                    "sessionId": session_id,
                                    "toolCall": {"title": "rodar ls (falso)"},
                                    "options": [
                                        {"optionId": "allow_once", "name": "Permitir"},
                                        {"optionId": "cancel", "name": "Cancelar"}
                                    ]
                                }
                            });
                            let _ = ws::send_text_server(&mut sock, &req.to_string());
                        }
                        let stop = if cancelled.swap(false, Ordering::SeqCst) {
                            "cancelled"
                        } else {
                            "end_turn"
                        };
                        result = serde_json::json!({ "sessionId": session_id, "stopReason": stop });
                    }
                }
                "session/close" => {
                    result = serde_json::json!({ "sessionId": session_id, "closed": true });
                }
                other => {
                    error = Some(serde_json::json!({
                        "code": -32601,
                        "message": format!("método desconhecido: {other}")
                    }));
                }
            }
            let payload = match error {
                Some(err) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": err }),
                None => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            };
            answered = true;
            let _ = ws::send_text_server(&mut sock, &payload.to_string());
            if exit_after_reply {
                return true;
            }
        }
    }
}

fn read_request_head(stream: &mut TcpStream, timeout: Duration) -> Option<String> {
    stream.set_read_timeout(Some(timeout)).ok();
    let mut bytes = Vec::new();
    let mut bytes_read = [0u8; 1];
    loop {
        match stream.read(&mut bytes_read) {
            Ok(0) => return None,
            Ok(1) => bytes.push(bytes_read[0]),
            Ok(_) => return None,
            Err(_) => return None,
        }
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
        if bytes.len() > 64 * 1024 {
            return None;
        }
    }
    Some(String::from_utf8_lossy(&bytes).to_string())
}
