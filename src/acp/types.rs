//! Tipos do JSON-RPC 2.0 usados pelo ACP, conforme os limites do que o
//! documento do ZeroClaw descreve (ver docs/DECISIONS.md, itens marcados VERIFY).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Versão do protocolo ACP esperada.
pub const PROTOCOL_VERSION: u64 = 1;
/// Código de erro para turno já em execução.
pub const ERR_SESSION_BUSY: i64 = -32002;

pub fn request(id: u64, method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

pub fn notification(method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// Classifica erros comuns de forma estável para a camada de aplicação.
    pub fn outcome(&self) -> ErrorOutcome {
        if self.code == ERR_SESSION_BUSY {
            return ErrorOutcome::SessionBusy;
        }
        let m = self.message.to_lowercase();
        if m.contains("expired")
            || m.contains("expirad")
            || m.contains("not found")
            || m.contains("unknown session")
            || m.contains("sessão desconhecida")
        {
            return ErrorOutcome::SessionExpiredOrUnknown;
        }
        ErrorOutcome::Other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorOutcome {
    SessionBusy,
    SessionExpiredOrUnknown,
    Other,
}

#[derive(Debug, Clone)]
pub enum SessionUpdate {
    AgentMessageChunk {
        text: String,
    },
    /// Parte de mensagem do USUÁRIO. Só aparece no replay de sessão
    /// (session/load): o daemon reenvia o histórico salvo, inclusive o que
    /// o usuário disse. Exibimos; nunca falamos nem reenviamos.
    UserMessageChunk {
        text: String,
    },
    ToolCall {
        title: String,
        status: String,
    },
    ToolCallUpdate {
        tool_call_id: String,
        status: String,
    },
    Unknown(String),
}

fn content_text(update: &Value) -> String {
    match update.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        Some(part @ Value::Object(_)) => part
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

impl SessionUpdate {
    /// Interpreta o objeto `update` de uma notificação `session/update`.
    /// Tipos desconhecidos viram `Unknown` e são ignorados sem erro.
    pub fn parse(update: &Value) -> SessionUpdate {
        let kind = update
            .get("sessionUpdate")
            .and_then(|k| k.as_str())
            .unwrap_or("");
        match kind {
            "agent_message_chunk" => SessionUpdate::AgentMessageChunk {
                text: content_text(update),
            },
            "user_message_chunk" => SessionUpdate::UserMessageChunk {
                text: content_text(update),
            },
            "tool_call" => SessionUpdate::ToolCall {
                title: update
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("ferramenta")
                    .to_string(),
                status: update
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pending")
                    .to_string(),
            },
            "tool_call_update" => SessionUpdate::ToolCallUpdate {
                tool_call_id: update
                    .get("toolCallId")
                    .and_then(|s| s.as_str())
                    .unwrap_or("?")
                    .to_string(),
                status: update
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("in_progress")
                    .to_string(),
            },
            other => SessionUpdate::Unknown(other.to_string()),
        }
    }
}

/// Mensagens recebidas do lado agente, já classificadas.
#[derive(Debug)]
pub enum Inbound {
    /// Resposta a uma requisição nossa.
    Response {
        id: u64,
        result: Result<Value, RpcError>,
    },
    /// Requisição agente → cliente (por exemplo `session/request_permission`).
    AgentRequest {
        id: Value,
        method: String,
        params: Value,
    },
    /// Notificação do agente.
    Notification { method: String, params: Value },
    /// Fluxo terminou (EOF ou erro de leitura).
    Closed(String),
}

/// Classifica um framed JSON de entrada.
pub fn parse_inbound(root: &Value) -> Result<Inbound, String> {
    let has_method = root.get("method").and_then(|m| m.as_str()).is_some();
    let has_id = root.get("id").is_some() && !root.get("id").unwrap().is_null();
    if has_method && has_id {
        let id = root.get("id").cloned().unwrap_or(Value::Null);
        let method = root["method"].as_str().unwrap_or("").to_string();
        let params = root.get("params").cloned().unwrap_or(Value::Null);
        Ok(Inbound::AgentRequest { id, method, params })
    } else if has_method {
        let method = root["method"].as_str().unwrap_or("").to_string();
        let params = root.get("params").cloned().unwrap_or(Value::Null);
        Ok(Inbound::Notification { method, params })
    } else {
        let id = match root.get("id") {
            Some(Value::Number(n)) => n.as_u64().ok_or("id numérico não suportado")?,
            _ => return Err("mensagem sem id numérico conhecido".into()),
        };
        if let Some(err) = root.get("error") {
            let err: RpcError = serde_json::from_value(err.clone())
                .map_err(|e| format!("erro JSON-RPC malformado: {e}"))?;
            Ok(Inbound::Response {
                id,
                result: Err(err),
            })
        } else {
            let result = root.get("result").cloned().unwrap_or(Value::Null);
            Ok(Inbound::Response {
                id,
                result: Ok(result),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_resposta_ok() {
        let line = json!({"jsonrpc":"2.0","id":3,"result":{"sessionId":"s-1"}});
        match parse_inbound(&line).unwrap() {
            Inbound::Response { id, result } => {
                assert_eq!(id, 3);
                assert_eq!(result.unwrap()["sessionId"], "s-1");
            }
            other => panic!("tipo errado: {other:?}"),
        }
    }

    #[test]
    fn parse_requisicao_do_agente() {
        let line = json!({
            "jsonrpc":"2.0","id":9,"method":"session/request_permission",
            "params":{"sessionId":"s","options":[{"optionId":"a","name":"ok"}]}
        });
        match parse_inbound(&line).unwrap() {
            Inbound::AgentRequest { method, .. } => {
                assert_eq!(method, "session/request_permission");
            }
            other => panic!("tipo errado: {other:?}"),
        }
    }

    #[test]
    fn parse_notificacao() {
        let line = json!({
            "jsonrpc":"2.0","method":"session/update",
            "params":{"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk",
            "content":{"type":"text","text":"olá"}}}
        });
        match parse_inbound(&line).unwrap() {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "session/update");
                let upd = SessionUpdate::parse(&params["update"]);
                match upd {
                    SessionUpdate::AgentMessageChunk { text } => assert_eq!(text, "olá"),
                    other => panic!("tipo errado: {other:?}"),
                }
            }
            other => panic!("tipo errado: {other:?}"),
        }
    }

    #[test]
    fn parse_update_do_usuario_historico() {
        let upd = json!({"sessionUpdate":"user_message_chunk",
            "content":{"type":"text","text":"linha do histórico"}});
        match SessionUpdate::parse(&upd) {
            SessionUpdate::UserMessageChunk { text } => assert_eq!(text, "linha do histórico"),
            other => panic!("tipo errado: {other:?}"),
        }
    }

    #[test]
    fn parse_erro_busy() {
        let line = json!({"jsonrpc":"2.0","id":4,
            "error":{"code":-32002,"message":"SESSION_BUSY"}});
        match parse_inbound(&line).unwrap() {
            Inbound::Response { result, .. } => {
                let err = result.unwrap_err();
                assert!(matches!(err.outcome(), ErrorOutcome::SessionBusy));
            }
            other => panic!("tipo errado: {other:?}"),
        }
    }

    #[test]
    fn parse_update_desconhecido_nao_panic() {
        let upd = json!({"sessionUpdate":"algo_novo_do_futuro"});
        assert!(matches!(
            SessionUpdate::parse(&upd),
            SessionUpdate::Unknown(_)
        ));
    }
}
