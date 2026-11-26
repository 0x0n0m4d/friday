//! Testes de fluxo do aplicativo contra o agente simulado: handshake,
//! persistência de sessão, fila de voz, sessão expirada e reconexão.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::json;

use sexta_feira::app::{App, AppEvent, Author, InitPhase, Status};
use sexta_feira::config::Config;
use sexta_feira::logger::Logger;
use sexta_feira::state::StateDir;
use sexta_feira::tui::ParsedKey;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Guarda que encerra o simulador quando o teste acaba; sem isso o processo
/// fica vivo segurando o stdout herdado e o cargo espera para sempre.
struct Simulador(Child);

impl Drop for Simulador {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn subir_simulador(mock_args: &[&str]) -> (Simulador, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sexta-mock-agente"));
    cmd.arg("--port").arg("0");
    for a in mock_args {
        cmd.arg(a);
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

fn make_app(mock_args: &[&str], root: &Path) -> (App, mpsc::Receiver<AppEvent>, Simulador) {
    let (sim, url) = subir_simulador(mock_args);
    let toml_text = format!(
        "[acp]\nurl = \"{url}\"\n\n\
         [push_to_talk]\nmode = \"toggle\"\n\n\
         [limits]\nreconnect_backoff_min_ms = 20\nreconnect_backoff_max_ms = 60\n"
    );
    let cfg: Config = toml::from_str(&toml_text).unwrap();
    let (tx, rx) = mpsc::channel::<AppEvent>();
    let state = StateDir::open(root.join("state")).unwrap();
    let log = Arc::new(Logger::open(&state.journal_dir()));
    let app = App::new(cfg, "casa".into(), Some(state), log, tx);
    (app, rx, sim)
}

fn pump_until(app: &mut App, mut pred: impl FnMut(&App) -> bool, contexto: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        app.pump_acp(Duration::from_millis(20));
        if pred(app) {
            return;
        }
    }
    panic!("condição não alcançada em {TIMEOUT:?}: {contexto}");
}

fn espera_ocioso(app: &mut App) {
    pump_until(
        app,
        |a| a.status == Status::Idle && a.pending_prompt.is_none(),
        "turno deveria terminar em ocioso",
    );
}

#[test]
fn handshake_persiste_sessao_e_prompt_flui() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    app.start();
    espera_ocioso(&mut app);

    assert_eq!(app.session_id.as_deref(), Some("s-mock-1"));
    let session_file: PathBuf = tmp.path().join("state").join("sessao-casa.json");
    assert!(session_file.exists(), "sessão deveria ser persistida");
    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::Info && i.text.contains("Sessão nova criada")),
        "diálogo: {:?}",
        app.chat
    );

    assert!(app.send_prompt("olá", false));
    assert_eq!(app.status, Status::Waiting);
    espera_ocioso(&mut app);
    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::User && i.text.contains("olá")),
        "mensagem do usuário deveria aparecer"
    );
    let response = app
        .chat
        .iter()
        .find(|i| i.author == Author::Agent)
        .expect("resposta do agente deveria existir");
    assert!(
        response.text.contains("resposta de teste"),
        "blocos deveriam se agrupar num item só: {}",
        response.text
    );
    app.teardown();
}

#[test]
fn retoma_sessao_salva_no_segundo_start() {
    let tmp = tempfile::tempdir().unwrap();
    {
        let (mut app, _rx, _sim) = make_app(&[], tmp.path());
        app.start();
        espera_ocioso(&mut app);
        app.teardown();
    }
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    app.start();
    espera_ocioso(&mut app);
    assert_eq!(app.session_id.as_deref(), Some("s-mock-1"));
    assert!(
        app.chat
            .iter()
            .any(|i| i.text.contains("Sessão anterior retomada")),
        "deveria retomar: {:?}",
        app.chat
    );
    app.teardown();
}

#[test]
fn recarga_mostra_historico_sem_falar_nem_repetir() {
    // Regra de casa da recarga: session/load devolve o histórico salvo; o
    // cliente mostra a sessão inteira na tela e SÓ isso — sem Piper falando,
    // sem reenvio, sem interação. A fala volta exclusivamente no próximo
    // prompt do usuário.
    let tmp = tempfile::tempdir().unwrap();
    {
        let (mut app, _rx, _sim) = make_app(&[], tmp.path());
        app.start();
        espera_ocioso(&mut app);
        app.teardown();
    }
    let (mut app, _rx, _sim) = make_app(&["--load-replay"], tmp.path());
    app.start();
    // O status inicial é Idle, então espera_ocioso não serve aqui: esperamos
    // o MARCADOR de retomada (chega depois do replay inteiro, na resposta do
    // session/load). O replay NÃO pode falar nem repetir nada.
    pump_until(
        &mut app,
        |a| {
            a.chat
                .iter()
                .any(|i| i.text.contains("Sessão anterior retomada"))
        },
        "recarga deveria responder com a sessão na tela",
    );
    espera_ocioso(&mut app);

    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::User && i.text.contains("capital do Brasil")),
        "histórico do USUÁRIO deveria aparecer na tela: {:?}",
        app.chat
    );
    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::Agent && i.text.contains("Brasília")),
        "histórico do AGENTE deveria aparecer na tela: {:?}",
        app.chat
    );
    assert_eq!(
        app.falas_enfileiradas, 0,
        "histórico NÃO deveria ir para o Piper: {:?}",
        app.chat
    );
    assert_eq!(
        app.chat
            .iter()
            .filter(|i| i.author == Author::Agent && i.text.contains("Brasília"))
            .count(),
        1,
        "histórico deveria aparecer UMA vez, sem repetição: {:?}",
        app.chat
    );

    // Fala volta só quando o USUÁRIO manda uma mensagem própria.
    assert!(app.send_prompt("pode falar de novo?", false));
    espera_ocioso(&mut app);
    assert!(
        app.falas_enfileiradas >= 1,
        "novo turno deveria voltar a falar: {:?}",
        app.chat
    );
    app.teardown();
}

#[test]
fn reconexao_nao_toca_na_tela_do_historico() {
    // Regra de casa da RECONEXÃO (diferente da recarga na abertura): a tela
    // é o log oficial das mensagens — "só um mostrador". Se a conexão cai e
    // o cliente retoma a sessão em pleno uso, o replay que o daemon devolve
    // no session/load é ENGOLIDO: nada entra na tela, nada fala, nada se
    // duplica. A conversa segue da última mensagem já exibida; a fala volta
    // só no próximo prompt do usuário.
    let tmp = tempfile::tempdir().unwrap();
    {
        let (mut app, _rx, _sim) = make_app(&[], tmp.path());
        app.start();
        espera_ocioso(&mut app);
        app.teardown();
    }
    let (mut app, _rx, _sim) = make_app(&["--load-replay"], tmp.path());
    app.start();
    pump_until(
        &mut app,
        |a| {
            a.chat
                .iter()
                .any(|i| i.text.contains("Sessão anterior retomada"))
        },
        "abertura deveria reconstruir a tela com o histórico",
    );
    espera_ocioso(&mut app);
    let brasilia = |a: &mut App| {
        a.chat
            .iter()
            .filter(|i| i.author == Author::Agent && i.text.contains("Brasília"))
            .count()
    };
    assert_eq!(brasilia(&mut app), 1, "histórico na tela UMA vez");

    // Queda em pleno uso + reconexão: é aí que o replay tem de ser engolido.
    app.connection_lost("queda simulada");
    app.reconnect_at = Some(Instant::now() - Duration::from_millis(1));
    app.on_tick();
    assert!(app.acp.is_some(), "deve reconectar quando o prazo passa");
    pump_until(
        &mut app,
        |a| a.session_id.is_some() && a.init_phase.is_none(),
        "reconexão deveria retomar a sessão",
    );
    espera_ocioso(&mut app);

    assert!(
        app.chat
            .iter()
            .any(|i| i.text.contains("a tela segue de onde parou")),
        "usuário deveria ser avisado de que a tela não recarrega: {:?}",
        app.chat
    );
    assert_eq!(
        brasilia(&mut app),
        1,
        "reconexão NÃO deveria reexibir o histórico: {:?}",
        app.chat
    );
    assert_eq!(
        app.falas_enfileiradas, 0,
        "replay engolido NÃO deveria falar: {:?}",
        app.chat
    );

    // A conversa segue da última mensagem: próximo prompt do usuário fala
    // de novo e a tela continua sem duplicação.
    assert!(app.send_prompt("seguir daqui", false));
    espera_ocioso(&mut app);
    assert!(
        app.falas_enfileiradas >= 1,
        "fala volta no prompt do usuário: {:?}",
        app.chat
    );
    assert_eq!(
        brasilia(&mut app),
        1,
        "histórico continua integral, sem repetição: {:?}",
        app.chat
    );
    app.teardown();
}

#[test]
fn voz_enquanto_ocupado_fica_na_fila_e_vai_depois() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    app.start();
    espera_ocioso(&mut app);

    assert!(app.send_prompt("primeira", false));
    let turn_id = app.pending_prompt.unwrap();
    app.on_transcribed(Ok("mensagem por voz".into()));
    assert_eq!(app.queued_voice.as_deref(), Some("mensagem por voz"));
    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::Info && i.text.contains("ficou na fila")),
        "usuário deveria ser avisado da fila"
    );

    app.handle_response_direct(turn_id, Ok(json!({ "stopReason": "end_turn" })));
    assert!(
        app.queued_voice.is_none(),
        "fila deveria ter sido esvaziada"
    );
    assert!(
        app.pending_prompt.is_some(),
        "mensagem da fila deveria ser enviada"
    );
    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::User && i.text.contains("mensagem por voz")),
        "mensagem de voz deveria aparecer no chat"
    );

    espera_ocioso(&mut app);
    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::Agent && i.text.contains("resposta de teste")),
        "segunda resposta deveria chegar"
    );
    app.teardown();
}

#[test]
fn sessao_expirada_recria_sessao_e_reenvia() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&["--prompt-unknown-session"], tmp.path());
    app.start();
    espera_ocioso(&mut app);

    assert!(app.send_prompt("primeira pergunta", false));
    espera_ocioso(&mut app);

    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::Info && i.text.contains("Sessão expirou ou não existe")),
        "usuário deveria ser avisado da expiração: {:?}",
        app.chat
    );
    let user_msgs = app
        .chat
        .iter()
        .filter(|i| i.author == Author::User && i.text.contains("primeira pergunta"))
        .count();
    assert_eq!(user_msgs, 2, "pergunta deveria ter sido reenviada");
    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::Agent && i.text.contains("resposta de teste")),
        "resposta do reenvio deveria chegar"
    );
    assert_eq!(app.session_id.as_deref(), Some("s-mock-1"));
    app.teardown();
}

#[test]
fn sessao_sempre_expirada_para_apos_um_reenvio() {
    // Regressão do loop de reenvio: quando o daemon recusa TODO prompt como
    // "sessão desconhecida/expirada", o cliente recriava a sessão e reenviava
    // a mesma mensagem sem fim (o chat enchia de repetições). A trava anti-loop
    // só permite UM reenvio automático; na segunda recusa ele desiste com aviso.
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&["--prompt-always-unknown-session"], tmp.path());
    app.start();
    espera_ocioso(&mut app);

    assert!(app.send_prompt("pergunta eterna", false));
    pump_until(
        &mut app,
        |a| {
            a.chat
                .iter()
                .any(|i| i.author == Author::Info && i.text.contains("desisto do reenvio"))
        },
        "cliente deveria desistir do reenvio (trava anti-loop)",
    );
    espera_ocioso(&mut app);

    let user_msgs = app
        .chat
        .iter()
        .filter(|i| i.author == Author::User && i.text.contains("pergunta eterna"))
        .count();
    assert_eq!(
        user_msgs, 2,
        "exatamente o envio original e o ÚNICO reenvio automático; chat: {:?}",
        app.chat
    );
    assert_eq!(
        app.session_id.as_deref(),
        Some("s-mock-1"),
        "sessão nova deveria ficar pronta para o próximo envio manual"
    );
    app.teardown();
}

#[test]
fn permissao_abre_modal_e_respode() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&["--permission"], tmp.path());
    app.start();
    espera_ocioso(&mut app);

    app.send_prompt("apaga logs", false);
    pump_until(
        &mut app,
        |a| a.pending_permission.is_some(),
        "modal de permissão deveria abrir",
    );
    assert_eq!(app.status, Status::Approval);
    let modal = app.pending_permission.as_ref().unwrap();
    assert!(modal.options.len() >= 2, "opções do simulador");
    assert_eq!(modal.selected, 0);

    app.on_key(ParsedKey::ModalDown);
    assert_eq!(app.pending_permission.as_ref().unwrap().selected, 1);

    app.on_key(ParsedKey::Submit);
    assert!(app.pending_permission.is_none(), "modal deveria fechar");
    espera_ocioso(&mut app);
    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::Agent && i.text.contains("resposta de teste")),
        "turno deveria terminar após a permissão"
    );
    app.teardown();
}

#[test]
fn permissao_cancelada_com_esc_termina_turno() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&["--permission"], tmp.path());
    app.start();
    espera_ocioso(&mut app);

    app.send_prompt("tarefa", false);
    pump_until(
        &mut app,
        |a| a.pending_permission.is_some(),
        "modal deveria abrir",
    );
    app.on_key(ParsedKey::Cancel);
    espera_ocioso(&mut app);
    assert!(app.pending_permission.is_none());
    assert_eq!(app.session_id.as_deref(), Some("s-mock-1"));
    app.teardown();
}

#[test]
fn reconecta_depois_de_perder_a_conexao() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    app.start();
    espera_ocioso(&mut app);

    app.connection_lost("teste de queda");
    assert_eq!(app.status, Status::Disconnected);
    assert!(app.acp.is_none());
    assert!(app.reconnect_at.is_some());
    assert!(
        app.chat.iter().any(|i| i.text.contains("Conexão perdida")),
        "usuário deveria ser avisado"
    );

    app.on_tick();
    assert!(app.acp.is_none(), "não deve reconectar antes do prazo");

    app.reconnect_at = Some(Instant::now() - Duration::from_millis(1));
    app.on_tick();
    assert!(app.acp.is_some(), "deve reconectar quando o prazo passa");
    assert_eq!(app.init_phase, Some(InitPhase::Initializing));
    espera_ocioso(&mut app);
    assert_eq!(app.session_id.as_deref(), Some("s-mock-1"));
    assert!(
        app.chat
            .iter()
            .any(|i| i.text.contains("Sessão anterior retomada")),
        "após reconectar, a sessão salva deve ser retomada: {:?}",
        app.chat
    );
    app.teardown();
}

/// Regressão: begin_new_session precisa trocar a fase do handshake de
/// Initializing para CreatingSession; antes, a fase ficava travada e cada
/// resposta de session/new era tratada como initialize e reenviada sem fim.
#[test]
fn regressao_begin_new_session_muda_a_fase() {
    use sexta_feira::acp::client::AcpClient;
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    let (_child, url) = subir_simulador(&[]);
    app.acp = Some(AcpClient::connect(&url, None, 5).unwrap());
    app.init_phase = Some(InitPhase::Initializing);
    app.begin_new_session();
    assert_eq!(app.init_phase, Some(InitPhase::CreatingSession));
    assert_eq!(app.session_phase, Some(InitPhase::CreatingSession));
    pump_until(
        &mut app,
        |a| a.session_id.is_some() && a.init_phase.is_none(),
        "resposta de session/new deveria fechar o handshake",
    );
    assert_eq!(app.session_id.as_deref(), Some("s-mock-1"));
    assert_eq!(app.status, Status::Idle);
    app.teardown();
}

/// O modo verboso mostra na tela os erros de praxe; sem ele, o mesmo evento
/// fica só no journal. O caso central: prompt engolido por falta de sessão.
#[test]
fn verbose_mostra_prompt_sem_sessao_e_padrao_esconde() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    assert!(!app.send_prompt("primeira tentativa", false));
    assert!(
        app.chat.iter().all(|i| i.author != Author::Diag),
        "sem --verbose nada de diag deve aparecer na tela: {:?}",
        app.chat
    );
    assert!(app.retry_text.is_some(), "mensagem fica guardada");

    app.set_verbose(true);
    assert!(!app.send_prompt("segunda tentativa", false));
    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::Diag && i.text.contains("não há sessão")),
        "com --verbose o engolimento deve aparecer: {:?}",
        app.chat
    );
    app.teardown();
}

/// Falha da linha do push-to-talk (antes silenciosa) vira [diag] em verboso.
#[test]
fn verbose_mostra_falha_da_fonte_da_tecla() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    app.set_verbose(true);
    app.on_key_source_failed("não foi possível abrir /dev/input/event9: x".into());
    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::Diag && i.text.contains("linha da tecla falhou")),
        "falha da fonte deveria aparecer: {:?}",
        app.chat
    );
    app.teardown();
}

/// Handshake sem resposta gera aviso [diag] a cada janela de timeout,
/// sem derrubar a conexão nem trocar de fase.
#[test]
fn verbose_avisa_handshake_sem_resposta() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    app.set_verbose(true);
    app.init_phase = Some(InitPhase::Initializing);
    let janela = Duration::from_secs(app.cfg.acp.handshake_timeout_secs.max(1));
    app.phase_started_at = Some(Instant::now() - janela);
    app.on_tick();
    assert!(
        app.chat
            .iter()
            .any(|i| i.author == Author::Diag && i.text.contains("handshake sem resposta")),
        "aviso de handshake sem resposta deveria aparecer: {:?}",
        app.chat
    );
    assert!(
        app.phase_started_at.is_some(),
        "o relógio da fase deve recomeçar para avisar de novo"
    );
    assert_eq!(app.init_phase, Some(InitPhase::Initializing));
    app.teardown();
}

/// A tela é um log: Ctrl+U/Ctrl+D rolam meia página por toque, a chegada de
/// mensagens NÃO arrasta a visualização de volta ao fundo, e o fundo só é
/// reengulado por Ctrl+D até o fim ou mensagem do próprio usuário.
#[test]
fn rolagem_meia_pagina_e_tela_estavel_como_log() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    // viewport de 24 linhas: meia página = 12
    app.viewport_rows.set(24);
    assert!(app.follow_tail);

    // Ctrl+U: destaca do fundo; offset é distância do FUNDO
    app.on_key(ParsedKey::ScrollUp);
    assert!(!app.follow_tail);
    assert_eq!(app.scroll_offset, 12);
    app.on_key(ParsedKey::ScrollUp);
    assert_eq!(app.scroll_offset, 24);

    // Ctrl+D: desce a mesma medida; ainda longe do fundo, continua solto
    app.on_key(ParsedKey::ScrollDown);
    assert_eq!(app.scroll_offset, 12);
    assert!(!app.follow_tail);

    // chegada de mensagem (info, como as do agente) NÃO volta a tela ao fundo
    app.info("linha nova que chega enquanto eu li até aqui".into());
    assert!(!app.follow_tail, "log não arrasta a tela");

    // Ctrl+D até o fundo reengula o acompanhamento
    app.on_key(ParsedKey::ScrollDown);
    assert_eq!(app.scroll_offset, 0);
    assert!(app.follow_tail);

    app.teardown();
}

/// Reminder removido há tempos: mensagem do próprio usuário volta ao fundo
/// (é ele falando), mas mensagens que chegam do agente não tocam na tela.
#[test]
fn mensagem_do_usuario_volta_ao_fundo_agente_nao() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    app.viewport_rows.set(24);

    // sessão tem de existir: sem handshake, o send_prompt cai fora antes de
    // tocar na tela (mensagem viraria reenvio pendente)
    app.start();
    espera_ocioso(&mut app);
    assert!(app.session_id.is_some());

    app.on_key(ParsedKey::ScrollUp);
    assert!(!app.follow_tail);

    // usuário fala → tela volta ao fundo
    app.send_prompt("procurando de novo", false);
    assert!(app.follow_tail, "própria mensagem reengula o fundo");

    // usuário rola de novo; chunk de agente chegando NÃO mexe
    app.on_key(ParsedKey::ScrollUp);
    assert!(!app.follow_tail);
    app.info("pedaço do agente chegando".into());
    assert!(!app.follow_tail);

    app.teardown();
}

/// Auto-send desligado (Ctrl+A): a transcrição NÃO sai sozinha — cai na
/// caixa de entrada para você conferir/editar e decidir o Enter.
#[test]
fn auto_send_desligado_deixa_a_transcricao_na_caixa() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    assert!(app.auto_send, "padrão vem de [ui].auto_send e é LIGADO");
    app.on_key(ParsedKey::ToggleAutoSend);
    assert!(!app.auto_send);

    app.on_transcribed(Ok("leia o arquivo de trabalho\n".into()));
    assert_eq!(
        app.input, "leia o arquivo de trabalho",
        "caixa recebeu a fala"
    );
    assert!(app.pending_prompt.is_none(), "nada foi enviado sozinho");
    assert!(app.session_id.is_none() && app.status == Status::Idle);
    app.teardown();
}

/// Auto-send ligado mantém o envio automático por voz: sem sessão, a fala
/// vira mensagem pendente de reenvio (mesmo caminho do Enter), nunca input.
#[test]
fn auto_send_ligado_mantem_envio_automatico() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    app.on_transcribed(Ok("busca aí".into()));
    assert!(
        app.retry_text.is_some(),
        "sem sessão: fica pendente de reenvio"
    );
    assert!(app.input.is_empty(), "não deve poluir a caixa");
    app.teardown();
}

/// Editor de Ctrl+G vem da config e vira argv puro (sem shell, sem invite).
#[test]
fn editor_da_config_vira_argv_sem_shell() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut app, _rx, _sim) = make_app(&[], tmp.path());
    app.cfg.ui.editor = Some("kak -l".into());
    assert_eq!(
        app.comando_editor(),
        Some(vec!["kak".to_string(), "-l".to_string()])
    );
    app.teardown();
}
