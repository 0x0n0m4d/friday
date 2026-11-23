//! Testes do cliente ACP contra o gateway simulado (sexta-mock-agente
//! falando WebSocket em 127.0.0.1).

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;

use sexta_feira::acp::client::AcpClient;
use sexta_feira::acp::types::{ErrorOutcome, Inbound, SessionUpdate};

/// Guarda que encerra o simulador quando o teste acaba; sem isso o processo
/// fica vivo segurando o stdout herdado e o cargo espera para sempre.
struct Simulador(Child);

impl Drop for Simulador {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_mock(extra: &[&str]) -> (Simulador, String) {
    spawn_mock_opts(&[], extra)
}

fn spawn_mock_opts(opts: &[&str], extra: &[&str]) -> (Simulador, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sexta-mock-agente"));
    cmd.arg("--port").arg("0");
    for o in opts {
        cmd.arg(o);
    }
    for e in extra {
        cmd.arg(e);
    }
    cmd.stdout(Stdio::piped());
    let mut child = cmd.spawn().expect("simulador deveria nascer");
    let stdout = child.stdout.take().expect("stdout do simulador");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("PORT=<n>");
    let port: u32 = line
        .trim()
        .strip_prefix("PORT=")
        .expect("simulador deveria anunciar a porta")
        .parse()
        .expect("porta numérica");
    (Simulador(child), format!("ws://127.0.0.1:{port}/acp"))
}

fn connect_err(url: &str, token: Option<&str>, secs: u64) -> String {
    match AcpClient::connect(url, token, secs) {
        Ok(_) => panic!("conexão deveria falhar"),
        Err(e) => e.0,
    }
}

fn wait_for(client: &mut AcpClient, timeout: Duration) -> Option<Inbound> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        if let Some(msg) = client.recv_timeout(Duration::from_millis(25)) {
            return Some(msg);
        }
    }
}

fn initialize(client: &mut AcpClient) {
    let id = client.send_request("initialize", json!({})).unwrap();
    let msg = wait_for(client, Duration::from_secs(5)).expect("sem resposta");
    match msg {
        Inbound::Response { id: rid, result } => {
            assert_eq!(rid, id);
            let v = result.unwrap();
            assert_eq!(v["protocolVersion"], 1);
        }
        other => panic!("resposta errada: {other:?}"),
    }
}

fn expect_session_new(client: &mut AcpClient) -> String {
    let id = client
        .send_request("session/new", json!({"agentAlias": "casa"}))
        .unwrap();
    let msg = wait_for(client, Duration::from_secs(5)).expect("sem sessão");
    match msg {
        Inbound::Response { id: rid, result } => {
            assert_eq!(rid, id);
            result.unwrap()["sessionId"].as_str().unwrap().to_string()
        }
        other => panic!("resposta errada: {other:?}"),
    }
}

#[test]
fn handshake_e_prompt_com_streaming() {
    let (_child, url) = spawn_mock(&[]);
    let mut client = AcpClient::connect(&url, None, 5).unwrap();
    initialize(&mut client);
    let session_id = expect_session_new(&mut client);

    let id = client
        .send_request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "olá"}),
        )
        .unwrap();
    let mut bytes = String::new();
    let mut saw_update = 0;
    let mut stop = None;
    while stop.is_none() {
        let msg = wait_for(&mut client, Duration::from_secs(5)).expect("turno não terminou");
        match msg {
            Inbound::Notification { method, params } => {
                if method == "session/update" {
                    saw_update += 1;
                    if let SessionUpdate::AgentMessageChunk { text } =
                        SessionUpdate::parse(&params["update"])
                    {
                        bytes.push_str(&text);
                    }
                }
            }
            Inbound::Response { id: rid, result } => {
                assert_eq!(rid, id);
                stop = Some(result.unwrap());
            }
            other => panic!("mensagem inesperada: {other:?}"),
        }
    }
    assert!(saw_update >= 2, "esperava streaming em partes");
    assert_eq!(stop.unwrap()["stopReason"], "end_turn");
    assert!(bytes.contains("resposta de teste"), "texto: {bytes}");
    client.terminate();
}

#[test]
fn ocupado_vira_erro_especifico() {
    let (_child, url) = spawn_mock(&["--busy"]);
    let mut client = AcpClient::connect(&url, None, 5).unwrap();
    initialize(&mut client);
    let id = client
        .send_request("session/prompt", json!({"sessionId": "s", "prompt": "x"}))
        .unwrap();
    match wait_for(&mut client, Duration::from_secs(5)).expect("sem resposta") {
        Inbound::Response { id: rid, result } => {
            assert_eq!(rid, id);
            let err = result.unwrap_err();
            assert!(matches!(err.outcome(), ErrorOutcome::SessionBusy));
            assert_eq!(err.code, -32002);
        }
        other => panic!("resposta errada: {other:?}"),
    }
    client.terminate();
}

#[test]
fn linha_malformada_nao_bate_o_cliente() {
    let (_child, url) = spawn_mock(&["--malformed"]);
    let mut client = AcpClient::connect(&url, None, 5).unwrap();
    let _ = client.send_request("initialize", json!({})).unwrap();
    let mut saw_closed = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(Inbound::Closed(reason)) = client.recv_timeout(Duration::from_millis(50)) {
            assert!(reason.contains("quadro"), "motivo: {reason}");
            saw_closed = true;
            break;
        }
    }
    assert!(saw_closed, "fechamento com motivo não chegou");
    client.terminate();
}

#[test]
fn permissao_vai_e_volta() {
    let (_child, url) = spawn_mock(&["--permission"]);
    let mut client = AcpClient::connect(&url, None, 5).unwrap();
    initialize(&mut client);
    let session_id = expect_session_new(&mut client);

    let id = client
        .send_request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "apaga logs"}),
        )
        .unwrap();
    let mut saw_request = false;
    let mut finished = false;
    while !finished {
        let msg = wait_for(&mut client, Duration::from_secs(5)).expect("turno não terminou");
        match msg {
            Inbound::Notification { .. } => {}
            Inbound::AgentRequest {
                id: req_id,
                method,
                params,
            } => {
                assert_eq!(method, "session/request_permission");
                let options = params["options"].as_array().unwrap();
                assert!(!options.is_empty());
                client
                    .reply_request(
                        &req_id,
                        json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}),
                    )
                    .unwrap();
                saw_request = true;
            }
            Inbound::Response { id: rid, result } => {
                assert_eq!(rid, id);
                assert_eq!(result.unwrap()["stopReason"], "end_turn");
                finished = true;
            }
            other => panic!("mensagem inesperada: {other:?}"),
        }
    }
    assert!(saw_request);
    client.terminate();
}

#[test]
fn sessoes_expiram_contra_load_ruim_com_fallback() {
    let (_child, url) = spawn_mock(&["--load-bad"]);
    let mut client = AcpClient::connect(&url, None, 5).unwrap();
    initialize(&mut client);

    for method in ["session/load", "session/resume"] {
        let _ = client
            .send_request(method, json!({"sessionId": "velha"}))
            .unwrap();
        match wait_for(&mut client, Duration::from_secs(5)).expect("sem resposta") {
            Inbound::Response { result, .. } => {
                let err = result.unwrap_err();
                assert!(
                    matches!(err.outcome(), ErrorOutcome::SessionExpiredOrUnknown),
                    "método {method} deveria classificar como sessão desconhecida"
                );
            }
            other => panic!("resposta errada: {other:?}"),
        }
    }

    let session_id = expect_session_new(&mut client);
    assert_eq!(session_id, "s-mock-1");
    client.terminate();
}

#[test]
fn sessao_desconhecida_no_primeiro_prompt() {
    let (_child, url) = spawn_mock(&["--prompt-unknown-session"]);
    let mut client = AcpClient::connect(&url, None, 5).unwrap();
    initialize(&mut client);
    let session_id = expect_session_new(&mut client);

    let id = client
        .send_request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "oi"}),
        )
        .unwrap();
    match wait_for(&mut client, Duration::from_secs(5)).expect("sem resposta") {
        Inbound::Response { id: rid, result } => {
            assert_eq!(rid, id);
            let err = result.unwrap_err();
            assert!(matches!(
                err.outcome(),
                ErrorOutcome::SessionExpiredOrUnknown
            ));
        }
        other => panic!("resposta errada: {other:?}"),
    }

    let id = client
        .send_request(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "oi de novo"}),
        )
        .unwrap();
    let mut stop = None;
    while stop.is_none() {
        let msg = wait_for(&mut client, Duration::from_secs(5)).expect("turno não terminou");
        match msg {
            Inbound::Notification { .. } => {}
            Inbound::Response { id: rid, result } => {
                assert_eq!(rid, id);
                stop = Some(result.unwrap());
            }
            other => panic!("mensagem inesperada: {other:?}"),
        }
    }
    assert_eq!(stop.unwrap()["stopReason"], "end_turn");
    client.terminate();
}

#[test]
fn conexao_termina_sem_deixar_orfaos() {
    let (_child, url) = spawn_mock(&[]);
    let mut client = AcpClient::connect(&url, None, 5).unwrap();
    initialize(&mut client);
    client.terminate();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !client.is_closed() {
        let _ = client.recv_timeout(Duration::from_millis(25));
    }
    assert!(client.is_closed());
    let _ = wait_for(&mut client, Duration::from_millis(100));
}

#[test]
fn token_vai_no_cabecalho_authorization() {
    let (_child, url) = spawn_mock_opts(&["--require-token", "segredo-do-usuario"], &[]);
    let mut client = AcpClient::connect(&url, Some("segredo-do-usuario"), 5).unwrap();
    initialize(&mut client);
    client.terminate();
}

#[test]
fn token_faltando_vira_erro_401() {
    let (_child, url) = spawn_mock(&["--require-token", "segredo-do-usuario"]);
    let err = connect_err(&url, None, 5);
    assert!(err.contains("401"), "erro: {err}");
}

#[test]
fn url_fora_do_loopback_e_http_sao_rejeitadas() {
    let err = connect_err("ws://10.0.0.1:42617/acp", None, 5);
    assert!(err.contains("loopback"), "erro: {err}");
    let err = connect_err("http://127.0.0.1:42617/acp", None, 5);
    assert!(err.contains("ws://"), "erro: {err}");
}

#[test]
fn gateway_inexistente_vira_erro_de_conexao() {
    let err = connect_err("ws://127.0.0.1:1/acp", None, 2);
    assert!(
        err.contains("conectar") || err.contains("recusou") || err.contains("handshake"),
        "erro: {err}"
    );
}
