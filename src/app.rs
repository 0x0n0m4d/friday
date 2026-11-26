//! Núcleo do aplicativo: junta teclado, ACP, push-to-talk, voz e tempos em
//! uma única máquina de estados dirigida por eventos (consumida pela TUI).

use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::acp::client::AcpClient;
use crate::acp::types::{ErrorOutcome, Inbound, RpcError, SessionUpdate};
use crate::config::Config;
use crate::logger::Logger;
use crate::ptt::{KeySource, KeySourceEvent};
use crate::recorder::{self, ActiveRecording, RecorderEvent};
use crate::sanitize;
use crate::state::StateDir;
use crate::stt;
use crate::stt::WhisperConfig;
use crate::tts::{prepare_for_speech, Speaker, SpeakerConfig, TtsEvent};
use crate::tui::ParsedKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Author {
    User,
    Agent,
    Info,
    /// Diagnóstico de modo verboso (`--verbose`): caminhos silenciosos e
    /// erros de praxe registrados no journal e, só aqui, mostrados na tela.
    Diag,
}

#[derive(Debug, Clone)]
pub struct ChatItem {
    pub author: Author,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Listening,
    Transcribing,
    Waiting,
    Speaking,
    Disconnected,
    Approval,
    Error(String),
}

#[derive(Debug)]
pub enum AppEvent {
    Key(ParsedKey),
    Acp(Inbound),
    KeySource(KeySourceEvent),
    RecordDone(Result<recorder::Recording, recorder::RecorderError>),
    Transcribed(Result<String, String>),
    TtsStarted,
    TtsFinished(Result<(), crate::tts::TtsError>),
    TtsInterrupted,
    Tick,
}

pub struct PermissionModal {
    pub id: Value,
    pub question: String,
    pub options: Vec<(String, String)>,
    pub selected: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitPhase {
    Initializing,
    Resuming,
    CreatingSession,
}

pub struct App {
    pub cfg: Config,
    pub agent_alias: String,
    pub state: Option<StateDir>,
    pub log: Arc<Logger>,
    pub events_tx: mpsc::Sender<AppEvent>,

    pub chat: Vec<ChatItem>,
    pub input: String,
    /// Posição do cursor na caixa de entrada, em CHARS (não bytes) a partir
    /// do início: setas/Home/End andam por caractere, UTF-8 seguro.
    pub input_cursor: usize,
    pub input_frozen_draft: Option<String>,
    pub status: Status,
    /// Distância (em linhas visuais) entre o fundo da tela e a última linha
    /// exibida. 0 = colado no fim. A chegada de mensagens NÃO mexe aqui:
    /// o chat é um log — quem move a visualização é o usuário.
    pub scroll_offset: usize,
    /// Verdadeiro quando a tela acompanha o fundo (estado inicial). Fica
    /// falso no primeiro Ctrl+U e volta só num Ctrl+D até o fim ou quando o
    /// usuário envia mensagem própria.
    pub follow_tail: bool,
    /// Altura visível do chat (sem bordas), medida no último quadro. O salto
    /// de meia página do Ctrl+U/Ctrl+D depende dela; antes do primeiro
    /// quadro vale 1, só para o salto existir.
    pub viewport_rows: std::cell::Cell<u16>,
    /// Envio automático da voz (Ctrl+A alterna em jogo): verdadeiro = a
    /// transcrição vira prompt direto como sempre foi; falso = a transcrição
    /// cai na caixa de entrada para você conferir/editar antes do Enter.
    /// Estado inicial vem de `[ui].auto_send` na configuração.
    pub auto_send: bool,

    pub acp: Option<AcpClient>,
    pub session_id: Option<String>,
    pub init_phase: Option<InitPhase>,
    pub session_phase: Option<InitPhase>,
    pub pending_prompt: Option<u64>,
    pub queued_voice: Option<String>,
    pub pending_permission: Option<PermissionModal>,

    pub recording: Option<ActiveRecording>,
    pub press_at: Option<Instant>,

    pub speaker: Option<Speaker>,
    pub speech_buffer: String,

    pub reconnect_attempt: u32,
    pub reconnect_at: Option<Instant>,
    pub retry_text: Option<String>,
    /// Anti-loop: no MÁXIMO um reenvio automático em sequência. Zera quando um
    /// turno completa; se a PRÓXIMA sessão também expirar, o cliente desiste do
    /// reenvio automático (a sessão era recriada e a mensagem reenviada sem
    /// limite, aparecendo como repetição infinita no chat).
    pub retry_used: bool,
    /// Recarga de sessão: enquanto ligado, chegadas de `session/update` são
    /// HISTÓRICO vindo do daemon (replay de session/load): entram só na tela.
    /// Sem Piper, sem reenvio, sem interação. Desliga somente quando o usuário
    /// manda uma mensagem própria — aí os próximos chunks voltam a ser falados.
    pub historico_silencioso: bool,
    /// `sessionId` que o cliente PEDIU no session/load em curso. O ZeroClaw
    /// 0.9.0 responde a carga com `{}` (SEM sessionId — o id da sessão é o
    /// mesmo que foi pedido; ver docs/DECISIONS.md D11), então guardamos aqui
    /// para também tratá-lo como localizado na resposta do handshake.
    pub pending_load_sid: Option<String>,
    /// Verdadeiro depois do PRIMEIRO handshake completo desta execução. Nova
    /// carga de sessão depois disso é RECONEXÃO — e reconexão não toca na
    /// tela: o chat é o log oficial das mensagens, e o replay do daemon sai
    /// de fora (a conversa segue da última mensagem já exibida).
    pub ja_carregou: bool,
    /// Reconexão em curso: durante o replay de session/load NADA entra na
    /// tela (não exibe, não fala) — ela já mostra a sessão inteira.
    pub engolir_replay: bool,
    /// Contador observável (testes/diag) de frases de fato enfileiradas para voz.
    pub falas_enfileiradas: usize,
    /// Verboso (`--verbose`): diags de praxe aparecem na tela; journal os
    /// registra sempre, com ou sem a opção.
    pub verbose: bool,
    /// Quando a fase atual do handshake (initialize/load/new) começou:
    /// alimenta o aviso de "sem resposta" no modo verboso.
    pub phase_started_at: Option<Instant>,
    pub quit: bool,
}

impl App {
    pub fn new(
        cfg: Config,
        agent_alias: String,
        state: Option<StateDir>,
        log: Arc<Logger>,
        events_tx: mpsc::Sender<AppEvent>,
    ) -> App {
        let auto_send = cfg.ui.auto_send;
        App {
            cfg,
            agent_alias,
            state,
            log,
            events_tx,
            chat: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            input_frozen_draft: None,
            status: Status::Idle,
            scroll_offset: 0,
            follow_tail: true,
            viewport_rows: std::cell::Cell::new(0),
            auto_send,
            acp: None,
            session_id: None,
            init_phase: None,
            session_phase: None,
            pending_prompt: None,
            queued_voice: None,
            pending_permission: None,
            recording: None,
            press_at: None,
            speaker: None,
            speech_buffer: String::new(),
            reconnect_attempt: 0,
            reconnect_at: None,
            retry_text: None,
            pending_load_sid: None,
            retry_used: false,
            historico_silencioso: false,
            ja_carregou: false,
            engolir_replay: false,
            falas_enfileiradas: 0,
            verbose: false,
            phase_started_at: None,
            quit: false,
        }
    }

    /// Liga/desliga o modo verboso antes de `start()`.
    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }

    /// Registra um achado de praxe: journal sempre, tela só em verboso.
    pub fn diag(&mut self, text: String) {
        self.log.log_event("diag", &[("texto", json!(text))]);
        if self.verbose {
            self.adiciona_chat(Author::Diag, text);
        }
    }

    // ---------- boot ----------

    pub fn start(&mut self) {
        self.info(format!(
            "Conectando ao daemon ZeroClaw (agente \u{201c}{}\u{201d})…",
            self.agent_alias
        ));
        if self.verbose {
            let token = match &self.cfg.acp.token_env {
                Some(name) => format!("token via env {name}"),
                None => "sem token configurado".to_string(),
            };
            self.diag(format!(
                "config efetiva: acp.url={}, {token}",
                self.cfg.acp.url.as_deref().unwrap_or("(não configurado)")
            ));
            self.diag("modo verboso ativo: erros de praxe aparecem como [diag]".into());
        }
        self.spawn_speaker();
        self.connect();
        self.spawn_key_source();
    }

    fn spawn_speaker(&mut self) {
        if !self.cfg.tts_enabled() {
            self.diag("voz (tts) desativada na configuração".into());
            return;
        }
        let (Some(piper_cmd), Some(player_cmd)) = (
            self.cfg.piper.command.clone(),
            self.cfg.player.command.clone(),
        ) else {
            self.diag(
                "voz habilitada, mas [piper].command e [player].command precisam existir; voz desativada"
                    .into(),
            );
            return;
        };
        let tmp_dir = self
            .state
            .as_ref()
            .and_then(|s| s.tmp_dir().ok())
            .unwrap_or_else(std::env::temp_dir);
        let cfg = SpeakerConfig {
            piper_command: piper_cmd,
            piper_output: self.cfg.piper.output.clone(),
            player_command: player_cmd,
            timeout_secs: self.cfg.piper.timeout_secs,
            max_wav_bytes: self.cfg.piper.max_wav_bytes,
            tmp_dir,
        };
        let (tts_tx, tts_rx) = mpsc::channel::<TtsEvent>();
        let events_tx = self.events_tx.clone();
        let tts_bridge = std::thread::Builder::new()
            .name("tts-bridge".into())
            .spawn(move || {
                while let Ok(ev) = tts_rx.recv() {
                    let forward = match ev {
                        TtsEvent::Started => AppEvent::TtsStarted,
                        TtsEvent::Finished(r) => {
                            AppEvent::TtsFinished(r.map_err(|e| crate::tts::TtsError(e.0)))
                        }
                        TtsEvent::Interrupted => AppEvent::TtsInterrupted,
                    };
                    if events_tx.send(forward).is_err() {
                        break;
                    }
                }
            });
        if let Err(e) = tts_bridge {
            self.diag(format!("fio tts-bridge não iniciou: {e}"));
        }
        self.speaker = Some(Speaker::new(cfg, tts_tx));
    }

    fn spawn_key_source(&mut self) {
        if self.cfg.push_to_talk.mode != "hold" {
            self.diag(format!(
                "[ptt] modo '{}' não usa evdev; a tecla vem do teclado da TUI",
                self.cfg.push_to_talk.mode
            ));
            return;
        }
        let (Some(device), Some(code)) = (self.cfg.ptt_device(), self.cfg.ptt_code()) else {
            self.diag(
                "[ptt] sem [push_to_talk].device/code no TOML: leitura da tecla desativada".into(),
            );
            return;
        };
        let source = crate::ptt::EvdevKeySource {
            device: device.clone(),
            code,
            max_record_secs: self.cfg.push_to_talk.max_record_secs,
        };
        let app_tx = self.events_tx.clone();
        let (ktx, krx) = mpsc::channel::<KeySourceEvent>();
        let evdev_thread = std::thread::Builder::new()
            .name("ptt-evdev".into())
            .spawn(move || {
                let mut src = source;
                let stop = Arc::new(AtomicBool::new(false));
                src.run(ktx, stop);
            });
        if let Err(e) = evdev_thread {
            self.diag(format!("[ptt] fio do evdev não iniciou: {e}"));
        }
        let bridge = std::thread::Builder::new()
            .name("ptt-bridge".into())
            .spawn(move || {
                while let Ok(ev) = krx.recv() {
                    if app_tx.send(AppEvent::KeySource(ev)).is_err() {
                        break;
                    }
                }
            });
        if let Err(e) = bridge {
            self.diag(format!("[ptt] fio da ponte não iniciou: {e}"));
        }
        self.diag(format!("[ptt] observando {device} tecla {code}"));
        self.log.log_event("ptt", &[("fonte", json!("evdev"))]);
    }

    fn connect(&mut self) {
        let Some(url) = self.cfg.acp.url.clone() else {
            self.diag("sem [acp].url no TOML: nada para conectar".into());
            return;
        };
        let token = self
            .cfg
            .acp
            .token_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|t| !t.trim().is_empty())
            .map(|t| t.trim().to_string());
        if token.is_none() {
            match &self.cfg.acp.token_env {
                Some(name) => self.diag(format!(
                    "[acp] variável {name} vazia ou ausente: conectando sem Authorization"
                )),
                None => self
                    .diag("[acp] token_env não configurado: conectando sem Authorization".into()),
            }
        }
        match AcpClient::connect(&url, token.as_deref(), self.cfg.acp.handshake_timeout_secs) {
            Ok(mut client) => {
                self.log.log_event("conectou", &[]);
                self.reconnect_attempt = 0;
                self.reconnect_at = None;
                match client.send_request("initialize", json!({})) {
                    Ok(_) => {
                        self.init_phase = Some(InitPhase::Initializing);
                        self.phase_started_at = Some(Instant::now());
                        self.diag("[acp] WebSocket aberto; initialize enviado".into());
                    }
                    Err(e) => {
                        client.terminate();
                        self.connection_lost(&e.to_string());
                        return;
                    }
                }
                self.acp = Some(client);
            }
            Err(e) => self.connection_lost(&e.to_string()),
        }
    }

    /// Aplica o estado pós-desconexão, com pagamento exponencial limitado.
    pub fn connection_lost(&mut self, reason: &str) {
        self.log.log_error("erro_conexao", reason);
        if let Some(acp) = self.acp.as_mut() {
            acp.terminate();
        }
        self.acp = None;
        self.status = Status::Disconnected;
        self.session_id = None;
        self.pending_prompt = None;
        self.init_phase = None;
        self.session_phase = None;
        self.phase_started_at = None;
        self.historico_silencioso = false;
        self.engolir_replay = false;
        self.pending_load_sid = None;
        self.speech_buffer.clear();
        let base = self.cfg.limits.reconnect_backoff_min_ms;
        let max = self.cfg.limits.reconnect_backoff_max_ms;
        let shift = self.reconnect_attempt.min(6);
        let backoff = base.saturating_mul(1u64 << shift).min(max);
        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
        self.reconnect_at = Some(Instant::now() + Duration::from_millis(backoff));
        self.info(format!("Conexão perdida: {reason}"));
    }

    pub fn info(&mut self, text: String) {
        self.adiciona_chat(Author::Info, text);
    }

    /// Coloca um item no log do chat. NÃO toca na visualização: o chat é um
    /// log — a chegada de mensagens nunca arrasta a tela; quem move a
    /// visualização é o usuário (Ctrl+U/Ctrl+D). Exceção única, decidida por
    /// quem chama: o envio de mensagem do próprio usuário volta ao fundo.
    fn adiciona_chat(&mut self, author: Author, text: String) {
        self.chat.push(ChatItem { author, text });
        self.trim_chat();
    }

    /// Metade da altura visível do chat (viewport medido no último quadro
    /// desenhado). Antes do primeiro quadro o viewport vale 1: o salto
    /// existe, é só curto — primeiro Ctrl+U já destaca a tela do fundo.
    pub fn meia_pagina(&self) -> usize {
        (self.viewport_rows.get() as usize / 2).max(1)
    }

    fn trim_chat(&mut self) {
        let max = self.cfg.limits.max_chat_items;
        if self.chat.len() > max {
            let drop_count = self.chat.len() - max;
            self.chat.drain(..drop_count);
        }
    }

    /// Encerra tudo em ordem (chamado no fim do programa).
    pub fn teardown(&mut self) {
        if let Some(rec) = self.recording.as_mut() {
            rec.stop();
        }
        if let Some(sp) = &self.speaker {
            sp.shutdown();
        }
        if let Some(acp) = self.acp.as_mut() {
            acp.terminate();
        }
    }

    // ---------- envio ----------

    /// Envia um prompt como texto sanitizado. Supõe turno ocioso.
    pub fn send_prompt(&mut self, text: &str, from_voice: bool) -> bool {
        let sanitized = sanitize::cap_chars(
            &sanitize::strip_escapes_and_control(text),
            self.cfg.acp.prompt_max_chars,
        );
        if sanitized.trim().is_empty() {
            self.status = Status::Idle;
            return false;
        }
        // O usuário está falando de novo: o que chegar daqui por diante é
        // resposta viva, não histórico (recarga de sessão volta a falar).
        self.historico_silencioso = false;
        // Sem lembretes injetados no payload: a chamada de ferramentas é
        // papel do daemon (prompt/protocolo do ZeroClaw), não nosso.
        let payload = sanitized.clone();
        let Some(session_id) = self.session_id.clone() else {
            self.retry_text = Some(sanitized);
            self.diag(
                "mensagem guardada para reenvio: ainda não há sessão (handshake incompleto?)"
                    .into(),
            );
            return false;
        };
        self.status = Status::Waiting;
        // Exceção única da regra do log: mensagem do PRÓPRIO usuário volta a
        // tela ao fundo — é ele falando, não o sistema mexendo na tela dele.
        self.adiciona_chat(
            Author::User,
            if from_voice {
                format!("{sanitized}  (por voz)")
            } else {
                sanitized.clone()
            },
        );
        self.follow_tail = true;
        let Some(acp) = self.acp.as_mut() else {
            self.retry_text = Some(sanitized);
            self.diag("mensagem guardada para reenvio: conexão inativa no momento".into());
            return false;
        };
        match acp.send_request(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": payload }),
        ) {
            Ok(id) => {
                self.pending_prompt = Some(id);
                true
            }
            Err(e) => {
                self.connection_lost(&e.to_string());
                false
            }
        }
    }

    /// Ordem de resolução do editor externo (Ctrl+G): primeiro `[ui].editor`,
    /// depois `$VISUAL`, depois `$EDITOR`. Tudo por configuração/ambiente —
    /// nada de editor fixo no código. Converte em argv puro (sem shell).
    pub fn comando_editor(&self) -> Option<Vec<String>> {
        let bruto = self
            .cfg
            .ui
            .editor
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("VISUAL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .or_else(|| {
                std::env::var("EDITOR")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })?;
        let argv: Vec<String> = bruto.split_whitespace().map(str::to_string).collect();
        (!argv.is_empty()).then_some(argv)
    }

    /// Byte offset do cursor no `input` (a partir do índice de chars).
    fn byte_do_cursor(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.input_cursor)
            .map_or(self.input.len(), |(b, _)| b)
    }

    /// Insere um char ONDE o cursor está (e o avança): editar no meio do
    /// texto tem que funcionar, não só colar no fim.
    fn inserir_no_cursor(&mut self, c: char) {
        let byte = self.byte_do_cursor();
        self.input.insert(byte, c);
        self.input_cursor += 1;
    }

    /// Anda o cursor `delta` chars (negativo = esquerda), preso em [0, len].
    fn mover_cursor(&mut self, delta: isize) {
        let len = self.input.chars().count() as isize;
        let alvo = self.input_cursor as isize + delta;
        self.input_cursor = alvo.clamp(0, len) as usize;
    }

    /// Cursor para o fim — usado quando texto chega de fora (editor, voz,
    /// colagem): a próxima tecla edita DEPOIS do que acabou de entrar.
    fn cursor_para_fim(&mut self) {
        self.input_cursor = self.input.chars().count();
    }

    /// Conteúdo que volta do editor externo (Ctrl+G) para a caixa de input.
    pub fn definir_input_do_editor(&mut self, texto: String) {
        self.input = Self::linha_unica(&texto);
        self.cursor_para_fim();
    }

    /// Texto que volta do editor externo ou da transcrição: a caixa é de uma
    /// linha, então nova linha vira espaço (com controles externos proibidos).
    pub fn linha_unica(texto: &str) -> String {
        let limpos = sanitize::strip_escapes_and_control(texto)
            .split('\n')
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        limpos
    }

    fn restore_draft(&mut self) {
        if let Some(draft) = self.input_frozen_draft.take() {
            self.input = draft;
            self.cursor_para_fim();
        }
    }

    // ---------- permissções ----------

    fn open_permission_modal(&mut self, id: Value, params: &Value) {
        let question = params
            .get("toolCall")
            .and_then(|t| t.get("title"))
            .and_then(|s| s.as_str())
            .unwrap_or("o agente quer usar uma ferramenta")
            .to_string();
        let mut options = Vec::new();
        if let Some(list) = params.get("options").and_then(|o| o.as_array()) {
            for opt in list {
                let oid = opt
                    .get("optionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = opt
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&oid)
                    .to_string();
                options.push((oid, name));
            }
        }
        if options.is_empty() {
            options.push(("permitir".into(), "Permitir".into()));
        }
        self.pending_permission = Some(PermissionModal {
            id,
            question,
            options,
            selected: 0,
        });
        self.status = Status::Approval;
        self.log
            .log_event("permissao", &[("fase", json!("pedida"))]);
    }

    pub fn answer_permission(&mut self, cancel: bool) {
        let Some(modal) = self.pending_permission.take() else {
            self.diag("resposta de aprovação sem modal aberto; nada feito".into());
            return;
        };
        self.status = Status::Idle;
        let Some(acp) = self.acp.as_ref() else {
            self.diag("resposta da permissão não enviada: conexão inativa".into());
            return;
        };
        let result = if cancel {
            json!({ "outcome": { "outcome": "cancelled" } })
        } else {
            let option_id = &modal.options[modal.selected].0;
            json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
        };
        self.log
            .log_event("permissao", &[("fase", json!("respondida"))]);
        if let Err(e) = acp.reply_request(&modal.id, result) {
            self.connection_lost(&e.to_string());
        }
    }

    // ---------- voz ----------

    /// Tecla de voz pressionada: corta fala, congela digitação, grava.
    pub fn on_key_press(&mut self) {
        if let Some(sp) = &self.speaker {
            sp.interrupt();
        }
        self.speech_buffer.clear();
        if self.status == Status::Speaking {
            self.status = Status::Idle;
        }
        self.input_frozen_draft = Some(std::mem::take(&mut self.input));
        self.input_cursor = 0;
        if self.recording.is_some() {
            return;
        }
        let Some(cmd) = self.cfg.recording.command.clone() else {
            self.status = Status::Error("sem [recording].command na configuração".into());
            return;
        };
        let tmp_dir = match self.state.as_ref().and_then(|s| s.tmp_dir().ok()) {
            Some(d) => d,
            None => std::env::temp_dir(),
        };
        let (rec_tx, rec_rx) = mpsc::channel::<RecorderEvent>();
        let app_tx = self.events_tx.clone();
        match recorder::start_recording(
            &cmd,
            self.cfg.push_to_talk.max_record_secs,
            &tmp_dir,
            rec_tx,
        ) {
            Ok(active) => {
                self.recording = Some(active);
                self.press_at = Some(Instant::now());
                self.status = Status::Listening;
                self.diag("[ptt] tecla pressionada: gravando".into());
                self.log.log_event("ptt", &[("fase", json!("press"))]);
                let bridge = std::thread::Builder::new()
                    .name("recorder-bridge".into())
                    .spawn(move || {
                        while let Ok(ev) = rec_rx.recv() {
                            let ev = match ev {
                                RecorderEvent::Done(r) => AppEvent::RecordDone(r),
                            };
                            if app_tx.send(ev).is_err() {
                                break;
                            }
                        }
                    });
                if let Err(e) = bridge {
                    self.diag(format!("fio recorder-bridge não iniciou: {e}"));
                }
            }
            Err(e) => {
                self.status = Status::Error(format!("gravador falhou: {e}"));
                self.log.log_error("erro_record", &e.to_string());
            }
        }
    }

    /// Tecla de voz solta: encerra a gravação e dispara a transcrição.
    pub fn on_key_release(&mut self) {
        let too_short = self.press_at.is_some_and(|t| {
            t.elapsed() < Duration::from_millis(self.cfg.push_to_talk.min_press_ms)
        });
        self.press_at = None;
        if too_short {
            if let Some(rec) = self.recording.as_mut() {
                rec.stop();
            }
            self.recording = None;
            self.status = Status::Idle;
            self.diag("[ptt] press mais curto que min_press_ms; descartado".into());
            self.restore_draft();
            return;
        }
        if let Some(rec) = self.recording.as_mut() {
            rec.stop();
            self.status = Status::Transcribing;
            self.diag("[ptt] tecla solta: transcrevendo".into());
            self.log.log_event("ptt", &[("fase", json!("release"))]);
        }
    }

    /// Após gravar: valida o WAV, roda o Whisper num fio e apaga o temporário.
    pub fn on_record_done(&mut self, result: Result<recorder::Recording, recorder::RecorderError>) {
        self.recording = None;
        match result {
            Ok(rec) => {
                self.status = Status::Transcribing;
                let whisper = WhisperConfig {
                    command: self.cfg.whisper.command.clone().unwrap_or_default(),
                    model: self.cfg.whisper.model.clone(),
                    language: self.cfg.whisper.language.clone(),
                    timeout_secs: self.cfg.whisper.timeout_secs,
                    max_chars: self.cfg.whisper.max_chars,
                    silence_markers: self.cfg.whisper.silence_markers.clone(),
                };
                let server = self.cfg.whisper.server_url.clone();
                let app_tx = self.events_tx.clone();
                let stt_thread = std::thread::Builder::new()
                    .name("stt".into())
                    .spawn(move || {
                        let outcome = run_stt(&whisper, &rec.path, server);
                        recorder::remove_recording(&rec.path);
                        let _ = app_tx.send(AppEvent::Transcribed(outcome));
                    });
                if let Err(e) = stt_thread {
                    self.diag(format!("fio de transcrição não iniciou: {e}"));
                    self.status = Status::Error("transcrição não iniciou".into());
                    self.log.log_error("erro_stt", &e.to_string());
                }
            }
            Err(e) => {
                self.status = Status::Error(format!("gravação inválida: {e}"));
                self.log.log_error("erro_record", &e.to_string());
                self.restore_draft();
            }
        }
    }

    /// Após transcrever: envia automaticamente ou informa vazio/erro.
    pub fn on_transcribed(&mut self, outcome: Result<String, String>) {
        self.restore_draft();
        match outcome {
            Ok(text) if text.trim().is_empty() => {
                self.info("Não detectei fala nessa gravação.".into());
                self.status = Status::Idle;
            }
            Ok(text) => {
                if !self.auto_send {
                    // auto-send desligado (Ctrl+A): a fala NÃO sai sozinha —
                    // entra na caixa para você conferir/editar (e decidir o
                    // momento). Se o agente estiver ocupado, melhor ainda.
                    self.input.push_str(&Self::linha_unica(&text));
                    self.cursor_para_fim();
                    self.status = Status::Idle;
                    return;
                }
                if self.pending_prompt.is_some() {
                    if self.queued_voice.is_none() {
                        self.queued_voice = Some(text);
                        self.info(
                            "O agente está ocupado; a mensagem de voz ficou na fila (uma, no máximo)."
                                .into(),
                        );
                    } else {
                        self.info("Fila tomada: a nova mensagem de voz foi descartada.".into());
                    }
                    self.status = Status::Waiting;
                } else {
                    self.send_prompt(&text, true);
                }
            }
            Err(e) => {
                self.status = Status::Error(format!("transcrição falhou: {e}"));
                self.log.log_error("erro_stt", &e);
            }
        }
    }

    /// Acumula texto falável e emite frases completas na fila.
    pub fn speak_accumulated(&mut self, chunk: &str) {
        self.speech_buffer.push_str(chunk);
        while let Some(pos) = find_sentence_end(&self.speech_buffer) {
            let sentence: String = self.speech_buffer.drain(..=pos).collect();
            self.enqueue_speech(&sentence);
        }
        // Modelo escrevendo corrido sem pontuação à vista: não segura a voz
        // até o fim do turno — corta no último espaço e fala desde já. O
        // daemon faz streaming; a voz precisa acompanhar o mesmo ritmo.
        if self.speech_buffer.chars().count() >= FALA_PARCIAL_CHARS {
            if let Some(pos) = self.speech_buffer.rfind(' ') {
                let parte: String = self.speech_buffer.drain(..=pos).collect();
                self.enqueue_speech(&parte);
            }
        }
    }

    /// Fechamento do turno: fala o resto que ficou no buffer.
    pub fn flush_speech(&mut self) {
        let rest = std::mem::take(&mut self.speech_buffer);
        self.enqueue_speech(&rest);
    }

    fn enqueue_speech(&mut self, sentence: &str) {
        let parts = prepare_for_speech(sentence);
        if parts.is_empty() {
            return;
        }
        self.falas_enfileiradas += parts.len();
        let Some(sp) = &self.speaker else { return };
        for part in parts {
            sp.speak(part);
        }
    }

    // ---------- fluxo ACP ----------

    /// Processa uma mensagem do agente. Chame de dentro do loop da TUI.
    pub fn on_acp(&mut self, inbound: Inbound) {
        match inbound {
            Inbound::Closed(reason) => self.connection_lost(&reason),
            Inbound::AgentRequest { id, method, params } => {
                if method == "session/request_permission" {
                    self.open_permission_modal(id, &params);
                } else {
                    let reply = self.acp.as_ref().map(|acp| {
                        acp.reply_error(&id, -32601, "método não suportado pelo cliente")
                    });
                    match reply {
                        None => self.diag(format!(
                            "requisição do agente sem conexão para responder: {method}"
                        )),
                        Some(Err(e)) => self.diag(format!(
                            "resposta de não suportado para {method} não enviada: {e}"
                        )),
                        Some(Ok(())) => {
                            self.diag(format!("requisição {method} respondida como não suportada"));
                        }
                    }
                }
            }
            Inbound::Notification { method, params } => self.on_notification(&method, &params),
            Inbound::Response { id, result } => self.on_response(id, result),
        }
    }

    fn on_notification(&mut self, method: &str, params: &Value) {
        if method != "session/update" {
            self.diag(format!(
                "notificação ignorada: {method} ({})",
                value_excerpt(params, 200)
            ));
            return;
        }
        let Some(update) = params.get("update") else {
            self.diag(format!(
                "session/update sem campo update: {}",
                value_excerpt(params, 200)
            ));
            return;
        };
        match SessionUpdate::parse(update) {
            SessionUpdate::AgentMessageChunk { text } => {
                if self.engolir_replay {
                    return; // reconexão: a tela é o log; replay não entra
                }
                let replay = self.historico_silencioso; // histórico entra na tela, nunca fala
                let chunk = match self.chat.last_mut() {
                    Some(last) if last.author == Author::Agent => {
                        last.text.push_str(&text);
                        text
                    }
                    _ => {
                        self.chat.push(ChatItem {
                            author: Author::Agent,
                            text: text.clone(),
                        });
                        text
                    }
                };
                self.trim_chat();
                if !replay {
                    self.speak_accumulated(&chunk);
                }
            }
            SessionUpdate::UserMessageChunk { text } => {
                if self.engolir_replay {
                    return; // reconexão: a linha do usuário também fica de fora
                }
                // Só chega em replay de session/load: linha do histórico do
                // próprio usuário. Exibe; mensagem de história não repete,
                // não fala e não progride fila de reenvio alguma.
                self.adiciona_chat(Author::User, text);
            }
            SessionUpdate::ToolCall { title, status } => {
                if self.engolir_replay {
                    return; // reconexão: a tela é o log; a linha não entra
                }
                self.adiciona_chat(Author::Info, format!("ferramenta: {title} — {status}"));
            }
            SessionUpdate::ToolCallUpdate {
                tool_call_id,
                status,
            } => {
                if self.engolir_replay {
                    return; // reconexão: nada corrompe o texto do histórico
                }
                if let Some(item) = self
                    .chat
                    .iter_mut()
                    .rev()
                    .find(|i| i.author == Author::Info && i.text.contains(&tool_call_id))
                {
                    item.text = format!("ferramenta: ({status}) {}", tool_call_id);
                }
            }
            SessionUpdate::Unknown(kind) => {
                self.log
                    .log_event("acp", &[("update_desconhecido", json!(kind))]);
                self.diag(format!("update desconhecido do agente: {kind}"));
            }
        }
    }

    fn on_response(&mut self, id: u64, result: Result<Value, RpcError>) {
        if self.init_phase.is_some() {
            self.on_handshake_response(result);
            return;
        }
        if self.pending_prompt == Some(id) {
            self.pending_prompt = None;
            match result {
                Ok(v) => {
                    let stop = v
                        .get("stopReason")
                        .and_then(|s| s.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.log.log_event("turno", &[("stop", json!(stop))]);
                    self.flush_speech();
                    if stop == "cancelled" {
                        self.info("Turno cancelado.".into());
                    }
                    self.status = Status::Idle;
                    self.retry_used = false; // turno completo: reenvio automático armado de novo
                    if let Some(queued) = self.queued_voice.take() {
                        self.send_prompt(&queued, true);
                    }
                }
                Err(e) => match e.outcome() {
                    ErrorOutcome::SessionBusy => {
                        self.info("O agente já estava ocupado; a mensagem não foi aceita.".into());
                        self.status = Status::Idle;
                    }
                    ErrorOutcome::SessionExpiredOrUnknown => {
                        if self.retry_used {
                            // Já houve um reenvio automático nesta sequência e
                            // o daemon recusou de novo: reenviar outra vez é
                            // o loop de permanência (sessão nova + mesma mensagem
                            // para sempre). Recupera a sessão, mas SEM reenviar.
                            self.info(
                                "A sessão expirou de novo; desisto do reenvio automático (a trava anti-loop só tenta uma vez). Rode com --new-session se persistir."
                                    .into(),
                            );
                            self.session_id = None;
                            self.begin_new_session();
                        } else {
                            self.retry_used = true;
                            self.info("Sessão expirou ou não existe; abrindo outra…".into());
                            self.retry_text = self.last_user_text();
                            self.session_id = None;
                            self.begin_new_session();
                        }
                    }
                    ErrorOutcome::Other => {
                        self.info(format!("Erro do daemon: {}", e.message));
                        self.status = Status::Idle;
                    }
                },
            }
            if self.pending_permission.is_some() {
                self.status = Status::Approval;
            }
            return;
        }
        if let Some(phase) = self.session_phase {
            self.on_session_response(id, result, phase);
            return;
        }
        self.diag(format!(
            "resposta sem requisição em aberto (id={id}): {}",
            match &result {
                Ok(v) => value_excerpt(v, 200),
                Err(e) => format!("erro {} ({})", e.code, e.message),
            }
        ));
    }

    /// Recarga de sessão: arma o modo "histórico mudo". A PRIMEIRA carga da
    /// execução limpa a tela para o replay reconstruir a sessão do zero;
    /// depois disso toda carga é RECONEXÃO — o acesso é apenas um aviso e o
    /// replay engolido é descartado, porque a tela (`chat`) é o log oficial
    /// das mensagens e a conversa segue da última linha já exibida.
    fn armar_replay(&mut self) {
        self.historico_silencioso = true;
        self.speech_buffer.clear();
        if self.ja_carregou {
            // Reconexão: a tela já mostra a sessão inteira; nada entra nela.
            self.engolir_replay = true;
            self.diag(
                "reconexão: replay do histórico descartado (a tela já mostra a sessão inteira)"
                    .into(),
            );
        } else {
            // Primeira carga da execução: replay reconstrói a tela.
            self.engolir_replay = false;
            self.chat.clear();
        }
    }

    /// Após falha de load/resume (ou ausência de sessão salva): sessão nova.
    pub fn begin_new_session(&mut self) {
        self.historico_silencioso = false; // sessão nova: não há histórico a esconder
        self.engolir_replay = false;
        self.init_phase = Some(InitPhase::CreatingSession);
        self.session_phase = Some(InitPhase::CreatingSession);
        self.phase_started_at = Some(Instant::now());
        let sent = self.acp.as_mut().and_then(|acp| {
            match acp.send_request("session/new", json!({ "agentAlias": self.agent_alias })) {
                Ok(_) => None,
                Err(e) => Some(e.to_string()),
            }
        });
        match sent {
            None => self.diag("session/new enviado".into()),
            Some(e) if self.acp.is_some() => self.connection_lost(&e),
            Some(_) => self.diag("session/new não enviado: sem conexão".into()),
        }
    }

    /// Durante o handshake: initialize → load/resume → session/new.
    fn on_handshake_response(&mut self, result: Result<Value, RpcError>) {
        match self.init_phase {
            Some(InitPhase::Initializing) => {
                if let Err(e) = result {
                    self.connection_lost(&format!("initialize falhou: {}", e.message));
                    return;
                }
                let v = result.unwrap_or(Value::Null);
                let version = v
                    .get("protocolVersion")
                    .and_then(|p| p.as_u64())
                    .unwrap_or(0);
                if version != crate::acp::types::PROTOCOL_VERSION {
                    self.info(format!(
                        "Aviso: versão do protocolo ACP é {version}, esperado {} — pode falhar.",
                        crate::acp::types::PROTOCOL_VERSION
                    ));
                }
                let audio = v
                    .pointer("/capabilities/promptCapabilities/audio")
                    .and_then(|a| a.as_bool())
                    .unwrap_or(false);
                self.log.log_event("capacidade", &[("audio", json!(audio))]);
                if audio {
                    self.info(
                        "O agente anuncia capacidade de áudio; este cliente segue mandando texto."
                            .into(),
                    );
                }
                let saved = match self
                    .state
                    .as_ref()
                    .map(|s| s.load_session(&self.agent_alias))
                {
                    None => {
                        self.diag("sem state_dir: sessão anterior não é retomada".into());
                        None
                    }
                    Some(Err(e)) => {
                        self.diag(format!("falha ao ler sessão salva: {e}"));
                        None
                    }
                    Some(Ok(saved)) => saved,
                };
                match saved {
                    Some(sid) if !sid.is_empty() => {
                        self.init_phase = Some(InitPhase::Resuming);
                        self.session_phase = Some(InitPhase::Resuming);
                        self.phase_started_at = Some(Instant::now());
                        // Regra da recarga: o daemon reenvia o histórico salvo
                        // como session/update ANTES de responder. O cliente
                        // reconstrói a tela com ele, mas NÃO fala e NÃO
                        // interage com nada — histórico é só contexto.
                        self.armar_replay();
                        self.diag(format!("sessão salva encontrada; tentando retomar ({sid})"));
                        // ZeroClaw 0.9.0 responde session/load com {} (sem
                        // sessionId): o id é o que estamos pedindo agora.
                        self.pending_load_sid = Some(sid.clone());
                        let params = json!({ "sessionId": sid.clone() });
                        let load_err = self.acp.as_mut().and_then(|acp| {
                            // VERIFY: os docs citam session/load e session/resume;
                            // tentamos load e, se o método não existir, resume.
                            match acp.send_request("session/load", params.clone()) {
                                Ok(_) => None,
                                Err(e) => Some(e.to_string()),
                            }
                        });
                        if let Some(e) = load_err {
                            self.diag(format!(
                                "session/load não foi enviado ({e}); tentando session/resume"
                            ));
                            let params = json!({ "sessionId": sid });
                            if let Some(e) = self.acp.as_mut().and_then(|acp| {
                                match acp.send_request("session/resume", params) {
                                    Ok(_) => None,
                                    Err(e) => Some(e.to_string()),
                                }
                            }) {
                                self.diag(format!("session/resume também não foi enviado: {e}"));
                            }
                        }
                    }
                    _ => self.begin_new_session(),
                }
            }
            Some(InitPhase::Resuming) | Some(InitPhase::CreatingSession) => {
                let phase = self.init_phase.unwrap();
                self.on_session_response(0, result, phase);
            }
            None => {}
        }
    }

    /// Respostas de session/load/resume/new.
    fn on_session_response(&mut self, _id: u64, result: Result<Value, RpcError>, phase: InitPhase) {
        match result {
            Ok(v) => {
                let respondido = v
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                // ZeroClaw 0.9.0: session/load responde {} (sem sessionId —
                // o id é o mesmo que o cliente pediu; `session/new` sim
                // devolve sessionId). O id pedido fica em pending_load_sid.
                let sid = if !respondido.is_empty() {
                    respondido
                } else if phase == InitPhase::Resuming {
                    self.pending_load_sid.take().unwrap_or_default()
                } else {
                    String::new()
                };
                self.pending_load_sid = None;
                if sid.is_empty() {
                    self.connection_lost("resposta de sessão sem sessionId");
                    return;
                }
                if phase == InitPhase::Resuming {
                    let reconexao = self.ja_carregou;
                    // Continua mudo: o daemon pode continuar expondo
                    // histórico após a resposta. A fala volta só no próximo
                    // prompt do usuário (send_prompt desarma a trava).
                    self.historico_silencioso = true;
                    self.engolir_replay = false; // replay terminou
                    if self.retry_text.take().is_some() {
                        self.diag(
                            "mensagem pendente descartada na retomada: recarga não repete mensagem"
                                .into(),
                        );
                    }
                    if reconexao {
                        self.info("Sessão anterior retomada; a tela segue de onde parou.".into());
                    } else {
                        self.info(
                            "Sessão anterior retomada (histórico na tela, sem repetir falas)."
                                .into(),
                        );
                    }
                } else {
                    self.info("Sessão nova criada.".into());
                    self.historico_silencioso = false;
                    self.engolir_replay = false;
                }
                // O primeiro handshake completo desta execução marca o limite:
                // qualquer session/load posterior é reconexão (replay engolido).
                self.ja_carregou = true;
                self.session_id = Some(sid.clone());
                if let Some(state) = self.state.as_ref() {
                    if let Err(e) = state.save_session(&self.agent_alias, &sid) {
                        self.diag(format!(
                            "falha ao salvar a sessão no disco: {e} (continuando sem persistência)"
                        ));
                    }
                }
                self.init_phase = None;
                self.session_phase = None;
                self.phase_started_at = None;
                self.status = Status::Idle;
                self.reconnect_at = None;
                if let Some(text) = self.retry_text.take() {
                    self.send_prompt(&text, false);
                }
            }
            Err(e) if phase == InitPhase::Resuming => {
                self.log
                    .log_error("erro", &format!("resume falhou: {}", e.message));
                self.info("Não deu para retomar a sessão anterior; abrindo outra…".into());
                self.begin_new_session();
            }
            Err(e) => {
                self.connection_lost(&format!("session/new falhou: {}", e.message));
            }
        }
    }

    /// Texto da última mensagem do usuário (para reenvio pós reconexão).
    fn last_user_text(&self) -> Option<String> {
        self.chat
            .iter()
            .rev()
            .find(|i| i.author == Author::User)
            .map(|i| i.text.trim_end_matches("  (por voz)").to_string())
    }

    // ---------- teclado ----------

    /// Digitação normal (chamado só quando a tecla de voz NÃO está mantida).
    pub fn on_key(&mut self, key: ParsedKey) {
        use ParsedKey::*;
        if self.pending_permission.is_some() {
            match key {
                Quit => self.quit = true,
                Cancel => self.answer_permission(true),
                Submit => self.answer_permission(false),
                ModalUp => {
                    if let Some(m) = self.pending_permission.as_mut() {
                        m.selected = m.selected.saturating_sub(1);
                    }
                }
                ModalDown => {
                    if let Some(m) = self.pending_permission.as_mut() {
                        m.selected = (m.selected + 1).min(m.options.len().saturating_sub(1));
                    }
                }
                _ => {}
            }
            return;
        }
        match key {
            Quit => self.quit = true,
            Cancel => self.on_cancel(),
            Submit => {
                if self.input.trim().is_empty() {
                    self.diag("Enter com caixa vazia: nada enviado".into());
                    return;
                }
                if self.pending_prompt.is_some() {
                    self.info(
                        "O agente ainda está respondendo; aguarde ou Esc para cancelar.".into(),
                    );
                    return;
                }
                let text = std::mem::take(&mut self.input);
                self.input_cursor = 0;
                self.send_prompt(&text, false);
            }
            ToggleAutoSend => {
                // Ctrl+A: a voz nasce para enviar; aqui você decide por rodada.
                // O estado fica visível na própria barra (glifo do microfone).
                self.auto_send = !self.auto_send;
                self.diag(if self.auto_send {
                    "[voz] envio automático LIGADO: transcrição vira prompt".into()
                } else {
                    "[voz] envio automático DESLIGADO: transcrição vai para a caixa".into()
                });
            }
            // Ctrl+G é tratado no laço da TUI (tui.rs): o editor roda com o
            // terminal do usuário, fora do painel — o App só guarda o rascunho.
            OpenEditor => {}
            ScrollUp => {
                // Ctrl+U: sobe meia página. A tela fica ONDE O USUÁRIO COLOU:
                // mensagens que chegam não arrastam a visualização de volta
                // (o chat é um log, não um vitrine que se rearranja).
                self.follow_tail = false;
                self.scroll_offset = self.scroll_offset.saturating_add(self.meia_pagina());
            }
            ScrollDown => {
                // Ctrl+D: desce meia página; chegar ao fundo reengula o
                // acompanhamento (novas mensagens voltam a rolar a tela).
                if self.scroll_offset <= self.meia_pagina() {
                    self.scroll_offset = 0;
                    self.follow_tail = true;
                } else {
                    self.scroll_offset -= self.meia_pagina();
                }
            }
            Backspace => {
                // Apaga o char ANTES do cursor (não mais o último do texto
                // todo): é o que faz um terminal de verdade.
                if self.input_cursor > 0 {
                    let byte = self.byte_do_cursor();
                    let ini = self.input[..byte]
                        .char_indices()
                        .next_back()
                        .map(|(b, _)| b)
                        .unwrap_or(0);
                    self.input.replace_range(ini..byte, "");
                    self.input_cursor -= 1;
                }
            }
            Delete => {
                // Apaga o char NA frente do cursor.
                let byte = self.byte_do_cursor();
                if let Some(c) = self.input[byte..].chars().next() {
                    self.input.replace_range(byte..byte + c.len_utf8(), "");
                }
            }
            CursorLeft => self.mover_cursor(-1),
            CursorRight => self.mover_cursor(1),
            Home => self.input_cursor = 0,
            End => self.cursor_para_fim(),
            KillWord => {
                // Ctrl+W (readline): apaga a palavra à esquerda do cursor,
                // com os espaços que a separam do resto.
                let chars: Vec<char> = self.input.chars().collect();
                let mut i = self.input_cursor;
                while i > 0 && chars[i - 1].is_whitespace() {
                    i -= 1;
                }
                let fim_palavra = i;
                while i > 0 && !chars[i - 1].is_whitespace() {
                    i -= 1;
                }
                let ini_byte: usize = chars[..i].iter().map(|c| c.len_utf8()).sum();
                let fim_byte: usize = chars[..fim_palavra].iter().map(|c| c.len_utf8()).sum();
                self.input.replace_range(ini_byte..fim_byte, "");
                self.input_cursor = i;
            }
            KillToEnd => {
                // Ctrl+K (readline): mata da posição do cursor até o fim.
                let byte = self.byte_do_cursor();
                self.input.truncate(byte);
            }
            InsertChar(c) => {
                if !c.is_control() {
                    self.inserir_no_cursor(c);
                }
            }
            Paste(text) => {
                for c in sanitize::strip_escapes_and_control(&text).chars() {
                    self.inserir_no_cursor(c);
                }
            }
            _ => {}
        }
    }

    fn on_cancel(&mut self) {
        if self.pending_permission.is_some() {
            self.answer_permission(true);
        } else if self.status == Status::Waiting && self.cfg.acp.esc_cancels {
            let session = self.session_id.clone().unwrap_or_default();
            if let Some(acp) = self.acp.as_ref() {
                let _ = acp.send_notification("session/cancel", json!({ "sessionId": session }));
            }
            self.status = Status::Idle;
        } else if self.status == Status::Speaking {
            if let Some(sp) = &self.speaker {
                sp.interrupt();
            }
            self.status = Status::Idle;
        } else {
            self.diag(format!(
                "Esc sem efeito no estado atual (status={:?})",
                self.status
            ));
        }
    }

    /// Drena as mensagens do agente para dentro do aplicativo (usado pela
    /// TUI e pelos testes). Devolve quantas mensagens foram processadas.
    pub fn pump_acp(&mut self, wait: Duration) -> usize {
        let mut n = 0;
        loop {
            if self.acp.is_none() {
                break;
            }
            let msg = {
                let acp = self.acp.as_mut().expect("verificado acima");
                acp.recv_timeout(wait)
            };
            match msg {
                Some(inbound) => {
                    self.on_acp(inbound);
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// Exposto para os testes: resposta direta sem passar pelo fio leitor.
    pub fn handle_response_direct(&mut self, id: u64, result: Result<Value, RpcError>) {
        let saved = self.init_phase;
        self.init_phase = None;
        self.on_response(id, result);
        self.init_phase = saved;
    }

    /// Reconexão em ticks (o tempo passou).
    pub fn on_tick(&mut self) {
        if self.acp.is_none() && self.reconnect_at.is_some_and(|t| Instant::now() >= t) {
            self.connect();
        }
        if let Some(started) = self.phase_started_at {
            let wait = Duration::from_secs(self.cfg.acp.handshake_timeout_secs.max(1));
            let elapsed = started.elapsed();
            if elapsed >= wait {
                self.diag(format!(
                    "handshake sem resposta há {}s ({}) — o cliente não derruba a conexão; observe o daemon",
                    elapsed.as_secs(),
                    phase_name(self.init_phase)
                ));
                self.phase_started_at = Some(Instant::now());
            }
        }
    }

    /// Erro de praxe da fonte da tecla: journal sempre, tela em verboso.
    pub fn on_key_source_failed(&mut self, reason: String) {
        self.log.log_error("erro_ptt", &reason);
        if self.verbose {
            self.adiciona_chat(
                Author::Diag,
                format!("[ptt] linha da tecla falhou: {reason}"),
            );
        }
    }
}

fn phase_name(phase: Option<InitPhase>) -> &'static str {
    match phase {
        Some(InitPhase::Initializing) => "aguardando resposta de initialize",
        Some(InitPhase::Resuming) => "aguardando resposta de session/load",
        Some(InitPhase::CreatingSession) => "aguardando resposta de session/new",
        None => "sem fase",
    }
}

fn value_excerpt(v: &Value, n: usize) -> String {
    let s = v.to_string();
    if s.chars().count() <= n {
        s
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

fn run_stt(
    whisper: &WhisperConfig,
    audio: &std::path::Path,
    server: Option<String>,
) -> Result<String, String> {
    match (&server, whisper.command.is_empty()) {
        (Some(url), _) => {
            stt::transcribe_server(url, audio, whisper.timeout_secs).map_err(|e| e.to_string())
        }
        (None, false) => stt::transcribe(whisper, audio).map_err(|e| e.to_string()),
        (None, true) => {
            Err("nem [whisper].command nem [whisper].server_url estão configurados".into())
        }
    }
    .map(|t| stt::strip_silence_markers(&t, &whisper.silence_markers).unwrap_or_default())
    .map(|t| sanitize::cap_chars(&t, whisper.max_chars))
}

/// Texto sem pontuação acumulado acima disso vira fala parcial (corte no
/// último espaço): com o Piper, pontuação pode demorar o turno inteiro a
/// aparecer e a voz ficaria muda até lá.
const FALA_PARCIAL_CHARS: usize = 240;

/// Procura o fim de sentença dentro do buffer; devolve o índice (em bytes).
fn find_sentence_end(buffer: &str) -> Option<usize> {
    for (idx, c) in buffer.char_indices() {
        if matches!(c, '.' | '!' | '?' | '\u{2026}') {
            return Some(idx);
        }
    }
    None
}

impl ChatItem {
    pub fn rendered(&self) -> String {
        sanitize::strip_escapes_and_control(&self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger::Logger;
    use crate::state::StateDir;

    /// App mínimo (sem daemon, sem dispositivos): só a máquina de estados
    /// para exercitar a edição da caixa de entrada.
    fn app_de_teste() -> (App, tempfile::TempDir) {
        let cfg: Config = toml::from_str("[acp]\nurl = \"ws://127.0.0.1:1/acp\"\n").unwrap();
        let (tx, _rx) = mpsc::channel::<AppEvent>();
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::open(dir.path().to_path_buf()).unwrap();
        let log = Arc::new(Logger::open(&state.journal_dir()));
        let app = App::new(cfg, "casa".into(), Some(state), log, tx);
        (app, dir)
    }

    fn digitar(app: &mut App, texto: &str) {
        for c in texto.chars() {
            app.on_key(ParsedKey::InsertChar(c));
        }
    }

    #[test]
    fn input_tem_cursor_setas_e_home_end_editam_no_meio() {
        let (mut app, _dir) = app_de_teste();
        digitar(&mut app, "abcdef");
        assert_eq!(app.input_cursor, 6);
        // anda três para a esquerda e insere no MEIO do texto
        app.on_key(ParsedKey::CursorLeft);
        app.on_key(ParsedKey::CursorLeft);
        app.on_key(ParsedKey::CursorLeft);
        app.on_key(ParsedKey::InsertChar('X'));
        assert_eq!(app.input, "abcXdef");
        assert_eq!(app.input_cursor, 4);
        // Backspace apaga o X (que está ATRÁS do cursor)
        app.on_key(ParsedKey::Backspace);
        assert_eq!(app.input, "abcdef");
        assert_eq!(app.input_cursor, 3);
        // Delete apaga o char À FRENTE do cursor
        app.on_key(ParsedKey::Delete);
        assert_eq!(app.input, "abcef");
        // Home insere no começo; End volta ao fim
        app.on_key(ParsedKey::Home);
        app.on_key(ParsedKey::InsertChar('>'));
        assert_eq!(app.input, ">abcef");
        app.on_key(ParsedKey::End);
        app.on_key(ParsedKey::InsertChar('!'));
        assert_eq!(app.input, ">abcef!");
        // as setas não passam das pontas
        app.on_key(ParsedKey::CursorLeft);
        for _ in 0..50 {
            app.on_key(ParsedKey::CursorLeft);
        }
        assert_eq!(app.input_cursor, 0);
        for _ in 0..50 {
            app.on_key(ParsedKey::CursorRight);
        }
        assert_eq!(app.input_cursor, app.input.chars().count());
    }

    #[test]
    fn ctrl_w_apaga_palavra_e_ctrl_k_mata_ate_o_fim() {
        let (mut app, _dir) = app_de_teste();
        digitar(&mut app, "um dois tres");
        app.on_key(ParsedKey::KillWord);
        assert_eq!(app.input, "um dois ");
        assert_eq!(app.input_cursor, 8);
        app.on_key(ParsedKey::Home);
        for _ in 0..3 {
            app.on_key(ParsedKey::CursorRight);
        }
        app.on_key(ParsedKey::KillToEnd);
        assert_eq!(app.input, "um ");
        assert_eq!(app.input_cursor, 3);
    }

    #[test]
    fn submit_limpa_a_caixa_e_zera_o_cursor() {
        let (mut app, _dir) = app_de_teste();
        digitar(&mut app, "olá, mundo");
        app.on_key(ParsedKey::Submit);
        assert!(app.input.is_empty());
        assert_eq!(app.input_cursor, 0);
        assert_eq!(app.retry_text.as_deref(), Some("olá, mundo"));
    }

    #[test]
    fn cursor_sobrevive_a_utf8_multibyte() {
        let (mut app, _dir) = app_de_teste();
        digitar(&mut app, "☕x");
        // cursor entre ☕ e x: backspace apaga o EMOJI inteiro (2 bytes),
        // nunca um meio-caractere que corromperia a String.
        app.on_key(ParsedKey::CursorLeft);
        app.on_key(ParsedKey::Backspace);
        assert_eq!(app.input, "x");
        assert_eq!(app.input_cursor, 0);
        app.on_key(ParsedKey::InsertChar('á'));
        assert_eq!(app.input, "áx");
    }
}
