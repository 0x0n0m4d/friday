//! Configuração do usuário (TOML). Nada aqui contém segredos: o token do
//! gateway vem do ambiente (nome configurado em [acp].token_env). Comandos
//! externos são arranjos de argumentos.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

// ---------- tipos ----------

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub push_to_talk: PushToTalk,
    #[serde(default)]
    pub recording: Recording,
    #[serde(default)]
    pub whisper: Whisper,
    #[serde(default)]
    pub piper: Piper,
    #[serde(default)]
    pub player: Player,
    #[serde(default)]
    pub acp: Acp,
    #[serde(default)]
    pub agent: Agent,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub log: Log,
    #[serde(default)]
    pub paths: Paths,
}

/// Comportamento que a sexta-feira impõe ao turno, antes do prompt sair.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    /// Prefixa um lembrete curto a cada turno: o modelo deve invocar a
    /// ferramenta de verdade em vez de responder com bash em blocos de
    /// markdown (falha comum em modelos gratuitos; sem isso nenhum modal de
    /// permissão aparece, porque nenhuma ferramenta foi pedida).
    #[serde(default = "default_true")]
    pub tool_reminder: bool,
    /// Sobrepõe o texto do lembrete (uma linha; ver DEFAULT_TOOL_REMINDER).
    #[serde(default)]
    pub tool_reminder_text: Option<String>,
}

impl Default for Agent {
    fn default() -> Self {
        Agent {
            tool_reminder: true,
            tool_reminder_text: None,
        }
    }
}

/// Texto padrão do lembrete de `[agent].tool_reminder`.
pub const DEFAULT_TOOL_REMINDER: &str = "Regra obrigatória: quando o pedido exigir uma ação real (criar, ler ou alterar arquivos, executar comandos, pesquisar etc.), invoque a ferramenta apropriada pelo protocolo de tool calls do ZeroClaw em vez de responder com comandos em blocos de código; nunca simule o resultado de uma ação que não executou.";

impl Agent {
    /// Linha a prefixar no prompt (None = não prefixar nada).
    pub fn reminder_line(&self) -> Option<&str> {
        if !self.tool_reminder {
            return None;
        }
        Some(
            self.tool_reminder_text
                .as_deref()
                .unwrap_or(DEFAULT_TOOL_REMINDER),
        )
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PushToTalk {
    /// "hold" (segurar = falar) ou "toggle" (tecla alterna start/stop).
    #[serde(default = "default_ptt_mode")]
    pub mode: String,
    /// Dispositivo evdev (ex.: /dev/input/event4). Exige permissão do grupo input.
    #[serde(default)]
    pub device: Option<String>,
    /// Código da tecla evdev (ex.: 100 = KEY_RIGHTALT). Exige --find-key para descobrir.
    #[serde(default)]
    pub code: Option<u16>,
    /// Padrão da tecla no modo toggle (ex.: "f8", "ctrl+g").
    #[serde(default = "default_ptt_fallback")]
    pub fallback_key: String,
    #[serde(default = "default_min_press_ms")]
    pub min_press_ms: u64,
    #[serde(default = "default_max_record_secs")]
    pub max_record_secs: u64,
}

impl Default for PushToTalk {
    fn default() -> Self {
        PushToTalk {
            mode: default_ptt_mode(),
            device: None,
            code: None,
            fallback_key: default_ptt_fallback(),
            min_press_ms: default_min_press_ms(),
            max_record_secs: default_max_record_secs(),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Recording {
    /// Comando de gravação. Contrato: grava WAV PCM16 mono 16 kHz.
    /// Placeholder obrigatório `{file}` substituído pelo caminho temporário.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default = "default_max_record_secs")]
    pub max_secs: u64,
}

impl Default for Recording {
    fn default() -> Self {
        Recording {
            command: None,
            max_secs: default_max_record_secs(),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Whisper {
    /// Modelo ggml multilíngue (nunca use um modelo ".en").
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_lang")]
    pub language: String,
    /// Comando de transcrição. Placeholders: {model}, {lang}, {audio}.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Alternativa: servidor whisper em loopback (POST do WAV, texto na
    /// resposta). VERIFY: API exata depende da instalação do whisper-server.
    #[serde(default)]
    pub server_url: Option<String>,
    #[serde(default = "default_stt_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_transcript_chars")]
    pub max_chars: usize,
    #[serde(default = "default_silence_markers")]
    pub silence_markers: Vec<String>,
}

impl Default for Whisper {
    fn default() -> Self {
        Whisper {
            model: None,
            language: default_lang(),
            command: None,
            server_url: None,
            timeout_secs: default_stt_timeout(),
            max_chars: default_max_transcript_chars(),
            silence_markers: default_silence_markers(),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Piper {
    /// Comando do piper nativo. Texto vai na entrada padrão.
    /// Placeholder `{file}` substituído pelo WAV temporário no modo "file".
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// "stdout" = WAV sai na saída padrão; "file" = piper grava em {file}.
    #[serde(default = "default_piper_output")]
    pub output: String,
    #[serde(default = "default_tts_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_max_wav_bytes")]
    pub max_wav_bytes: usize,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for Piper {
    fn default() -> Self {
        Piper {
            command: None,
            output: default_piper_output(),
            timeout_secs: default_tts_timeout(),
            max_wav_bytes: default_max_wav_bytes(),
            enabled: true,
        }
    }
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Player {
    /// Comando do reprodutor de áudio. Placeholder `{file}`:
    /// no modo stdout do piper é substituído por `/dev/stdin`;
    /// no modo file é substituído pelo caminho do WAV temporário.
    #[serde(default)]
    pub command: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Acp {
    /// URL WebSocket do endpoint /acp do gateway do daemon em execução
    /// (ex.: "ws://127.0.0.1:42617/acp"). A porta vem daqui, nunca do código.
    /// Única forma suportada de transporte — decisão em docs/DECISIONS.md.
    /// VERIFY: endpoint confirmado nos docs e no código do ZeroClaw datados
    /// de 2026-11-21; confira com a sua versão do daemon.
    #[serde(default)]
    pub url: Option<String>,
    /// Alias padrão do agente para esta conexão; vai na URL como ?agent=<alias>.
    #[serde(default)]
    pub agent: Option<String>,
    /// Nome da variável de ambiente que guarda o token Bearer do gateway.
    /// O token em si nunca fica no arquivo de configuração.
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default = "default_handshake_timeout")]
    pub handshake_timeout_secs: u64,
    #[serde(default = "default_prompt_max_chars")]
    pub prompt_max_chars: usize,
    /// Cancela o turno do agente com Esc? (padrão: sim)
    #[serde(default = "default_true")]
    pub esc_cancels: bool,
}

impl Default for Acp {
    fn default() -> Self {
        Acp {
            url: None,
            agent: None,
            token_env: None,
            handshake_timeout_secs: default_handshake_timeout(),
            prompt_max_chars: default_prompt_max_chars(),
            esc_cancels: true,
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "default_backoff_min")]
    pub reconnect_backoff_min_ms: u64,
    #[serde(default = "default_backoff_max")]
    pub reconnect_backoff_max_ms: u64,
    /// Máximo de eventos de conversa mantidos em memória.
    #[serde(default = "default_max_chat_items")]
    pub max_chat_items: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            reconnect_backoff_min_ms: default_backoff_min(),
            reconnect_backoff_max_ms: default_backoff_max(),
            max_chat_items: default_max_chat_items(),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Log {
    #[serde(default = "default_retain_days")]
    pub retain_days: u32,
    #[serde(default = "default_max_log_bytes")]
    pub max_file_bytes: u64,
}

impl Default for Log {
    fn default() -> Self {
        Log {
            retain_days: default_retain_days(),
            max_file_bytes: default_max_log_bytes(),
        }
    }
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    /// Sobrepõe o diretório de estado (padrão: XDG state).
    #[serde(default)]
    pub state_dir: Option<String>,
}

// ---------- defaults ----------

fn default_true() -> bool {
    true
}
fn default_ptt_mode() -> String {
    "hold".into()
}
fn default_ptt_fallback() -> String {
    "f8".into()
}
fn default_min_press_ms() -> u64 {
    300
}
fn default_max_record_secs() -> u64 {
    60
}
fn default_lang() -> String {
    "pt".into()
}
fn default_stt_timeout() -> u64 {
    120
}
fn default_max_transcript_chars() -> usize {
    4000
}
fn default_silence_markers() -> Vec<String> {
    vec![
        "[BLANK_AUDIO]".into(),
        "[SILENCE]".into(),
        "[NO SPEECH]".into(),
        "[SILÊNCIO]".into(),
    ]
}
fn default_piper_output() -> String {
    "stdout".into()
}
fn default_tts_timeout() -> u64 {
    60
}
fn default_max_wav_bytes() -> usize {
    64 * 1024 * 1024
}
fn default_handshake_timeout() -> u64 {
    10
}
fn default_prompt_max_chars() -> usize {
    8000
}
fn default_backoff_min() -> u64 {
    1000
}
fn default_backoff_max() -> u64 {
    30000
}
fn default_max_chat_items() -> usize {
    500
}
fn default_retain_days() -> u32 {
    30
}
fn default_max_log_bytes() -> u64 {
    10 * 1024 * 1024
}

// ---------- carregamento e validação ----------

#[derive(Debug)]
pub struct ConfigError(pub Vec<String>);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for line in &self.0 {
            writeln!(f, "{line}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigError {}

/// Caminho padrão da configuração: ~/.config/sexta-feira/sexta-feira.toml.
pub fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("friday").join("config.toml"))
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        ConfigError(vec![format!(
            "não foi possível ler {}: {e}",
            path.display()
        )])
    })?;
    check_file_mode(path)?;
    let cfg: Config = toml::from_str(&text)
        .map_err(|e| ConfigError(vec![format!("configuração inválida: {e}")]))?;
    validate(&cfg)?;
    Ok(cfg)
}

/// O arquivo não tem segredos, mas não pode estar gravável por outros.
fn check_file_mode(path: &Path) -> Result<(), ConfigError> {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o022 != 0 {
            return Err(ConfigError(vec![format!(
                "{} tem permissões largas demais ({:o}); use 0600 ou 0640",
                path.display(),
                mode & 0o777
            )]));
        }
    }
    Ok(())
}

fn has_placeholder(cmd: &[String], ph: &str) -> bool {
    cmd.iter().any(|a| a.contains(ph))
}

pub fn validate(cfg: &Config) -> Result<(), ConfigError> {
    let mut errs = Vec::new();
    let mode_ok = cfg.push_to_talk.mode == "hold" || cfg.push_to_talk.mode == "toggle";
    if !mode_ok {
        errs.push(format!(
            "[push_to_talk].mode = {:?} inválido: use \"hold\" ou \"toggle\"",
            cfg.push_to_talk.mode
        ));
    }
    if cfg.push_to_talk.mode == "hold" && cfg.ptt_device().is_none() {
        errs.push(
            "[push_to_talk].device é obrigatório no modo \"hold\" \
             (descubra com `sexta-feira --find-key`)"
                .into(),
        );
    }
    if cfg.push_to_talk.mode == "hold" && cfg.ptt_code().is_none() {
        errs.push(
            "[push_to_talk].code é obrigatório no modo \"hold\" \
             (descubra com `sexta-feira --find-key`)"
                .into(),
        );
    }
    if cfg.push_to_talk.min_press_ms > 5_000 {
        errs.push("[push_to_talk].min_press_ms > 5000 não faz sentido".into());
    }
    if cfg.push_to_talk.max_record_secs == 0 || cfg.push_to_talk.max_record_secs > 600 {
        errs.push("[push_to_talk].max_record_secs deve estar entre 1 e 600".into());
    }

    if let Some(cmd) = &cfg.recording.command {
        if cmd.is_empty() {
            errs.push("[recording].command vazio".into());
        }
        if !has_placeholder(cmd, "{file}") {
            errs.push(
                "[recording].command precisa do placeholder {file} \
                 para o caminho do WAV temporário"
                    .into(),
            );
        }
    }

    if let Some(cmd) = &cfg.whisper.command {
        if cmd.is_empty() {
            errs.push("[whisper].command vazio".into());
        }
        if !has_placeholder(cmd, "{audio}") {
            errs.push(
                "[whisper].command precisa do placeholder {audio} para o arquivo gravado".into(),
            );
        }
        if cfg.whisper.model.is_none() && cmd.iter().any(|a| a.contains("{model}")) {
            errs.push("[whisper].model falta para o placeholder {model}".into());
        }
        if cfg.whisper.language.contains(".en") {
            errs.push(
                "[whisper].language menciona um modelo \".en\"; use um modelo multilíngue \
                 e um código de idioma (por exemplo \"pt\")"
                    .into(),
            );
        }
    }
    if cfg.whisper.command.is_none() && cfg.whisper.server_url.is_none() {
        errs.push(
            "defina [whisper].command (transcrição por comando) ou [whisper].server_url".into(),
        );
    }

    if let Some(cmd) = &cfg.piper.command {
        if cmd.is_empty() {
            errs.push("[piper].command vazio".into());
        }
        match cfg.piper.output.as_str() {
            "stdout" => {
                if has_placeholder(cmd, "{file}") {
                    errs.push(
                        "com [piper].output = \"stdout\", o comando não deve ter {file}".into(),
                    );
                }
            }
            "file" => {
                if !has_placeholder(cmd, "{file}") {
                    errs.push("com [piper].output = \"file\", o comando precisa de {file}".into());
                }
            }
            other => {
                errs.push(format!(
                    "[piper].output = {other:?} inválido: use \"stdout\" ou \"file\""
                ));
            }
        }
    }

    if let Some(cmd) = &cfg.player.command {
        if cmd.is_empty() {
            errs.push("[player].command vazio".into());
        }
        if !has_placeholder(cmd, "{file}") {
            errs.push("[player].command precisa do placeholder {file}".into());
        }
    }
    if cfg.piper.command.is_some() && cfg.player.command.is_none() {
        errs.push("defina [player].command para ouvir o áudio do piper".into());
    }

    if let Some(url) = cfg
        .acp
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        if let Err(e) = crate::acp::ws::parse_ws_url(url) {
            errs.push(format!("[acp].url inválida: {e}"));
        }
    } else {
        errs.push(
            "defina [acp].url (por exemplo \"ws://127.0.0.1:42617/acp\") para o gateway do daemon"
                .into(),
        );
    }
    if let Some(agent) = cfg
        .acp
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        if agent.contains(' ') || agent.contains('?') || agent.contains('&') {
            errs.push("[acp].agent não pode conter espaços, ? ou &".into());
        }
    }
    if let Some(env) = cfg
        .acp
        .token_env
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        if !env
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            errs.push(
                "[acp].token_env deve ser um nome de variável de ambiente (A-Z, dígitos e _)"
                    .into(),
            );
        }
    }

    if cfg.acp.prompt_max_chars < 128 {
        errs.push("[acp].prompt_max_chars muito pequeno (mínimo 128)".into());
    }
    if cfg.limits.max_chat_items < 10 {
        errs.push("[limits].max_chat_items muito pequeno (mínimo 10)".into());
    }
    if cfg.log.retain_days == 0 || cfg.log.retain_days > 365 {
        errs.push("[log].retain_days deve estar entre 1 e 365".into());
    }
    if cfg.log.max_file_bytes < 1024 {
        errs.push("[log].max_file_bytes muito pequeno".into());
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(ConfigError(errs))
    }
}

impl Config {
    pub fn ptt_device(&self) -> Option<String> {
        self.push_to_talk.device.clone().filter(|s| !s.is_empty())
    }
    pub fn ptt_code(&self) -> Option<u16> {
        self.push_to_talk.code
    }
    pub fn tts_enabled(&self) -> bool {
        self.piper.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const BASE: &str = r#"
[push_to_talk]
mode = "hold"
device = "/dev/input/event4"
code = 100

[recording]
command = ["/usr/bin/fake-rec", "{file}"]

[whisper]
command = ["/usr/bin/fake-whisper", "-m", "{model}", "-l", "pt", "-f", "{audio}"]
model = "/opt/ggml/base.bin"

[piper]
command = ["/usr/bin/fake-piper", "--model", "/opt/voz.onnx"]
output = "stdout"

[player]
command = ["/usr/bin/fake-player", "{file}"]

[acp]
url = "ws://127.0.0.1:42617/acp"
# agent = "casa"              # opcional: ?agent=<alias> na URL
# token_env = "ZEROCLAW_ACP_BRIDGE_TOKEN"  # nome da variável de ambiente com o token Bearer do gateway (nunca o token!)
"#;

    #[test]
    fn configuração_baseline_passa() {
        let cfg: Config = toml::from_str(BASE).unwrap();
        validate(&cfg).unwrap();
    }

    #[test]
    fn campo_desconhecido_sera_erro() {
        let bad = "\n[push_to_talk]\nmode = \"toggle\"\nkoko = 1\n";
        let r: Result<Config, _> = toml::from_str(bad);
        assert!(r.is_err());
        let msg = r.err().unwrap().to_string();
        assert!(msg.contains("koko"), "mensagem: {msg}");
    }

    #[test]
    fn sem_whisper_e_erro() {
        let mut cfg: Config = toml::from_str(BASE).unwrap();
        cfg.whisper.command = None;
        let errs = validate(&cfg).unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("[whisper]")));
    }

    #[test]
    fn hold_sem_codigo_e_erro() {
        let mut cfg: Config = toml::from_str(BASE).unwrap();
        cfg.push_to_talk.code = None;
        let errs = validate(&cfg).unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("push_to_talk")));
        cfg.push_to_talk.mode = "toggle".into();
        validate(&cfg).unwrap(); // toggle não exige dispositivo
    }

    #[test]
    fn idioma_bilingue_proibido() {
        let mut cfg: Config = toml::from_str(BASE).unwrap();
        cfg.whisper.language = "pt.en".into();
        let errs = validate(&cfg).unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains(".en")));
    }

    #[test]
    fn player_faltante_e_erro() {
        let mut cfg: Config = toml::from_str(BASE).unwrap();
        cfg.player.command = None;
        let errs = validate(&cfg).unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("[player]")));
    }

    #[test]
    fn sem_url_mata_a_validação() {
        let mut cfg: Config = toml::from_str(BASE).unwrap();
        cfg.acp.url = None;
        let errs = validate(&cfg).unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("[acp].url")));
    }

    #[test]
    fn url_sem_loopback_e_erro() {
        let mut cfg: Config = toml::from_str(BASE).unwrap();
        cfg.acp.url = Some("ws://10.0.0.1:42617/acp".into());
        let errs = validate(&cfg).unwrap_err();
        assert!(
            errs.0.iter().any(|e| e.contains("loopback")),
            "erros: {:?}",
            errs.0
        );
    }

    #[test]
    fn url_wss_e_erro() {
        let mut cfg: Config = toml::from_str(BASE).unwrap();
        cfg.acp.url = Some("wss://127.0.0.1:42617/acp".into());
        let errs = validate(&cfg).unwrap_err();
        assert!(errs.0.iter().any(|e| e.contains("wss://")));
    }

    #[test]
    fn lembrete_de_ferramenta_ligado_por_padrao() {
        let cfg: Config = toml::from_str(BASE).unwrap();
        assert!(cfg.agent.tool_reminder);
        assert_eq!(cfg.agent.reminder_line(), Some(DEFAULT_TOOL_REMINDER));
    }

    #[test]
    fn lembrete_de_ferramenta_desligado_e_texto_proprio() {
        let com_agente = format!(
            "{BASE}\n[agent]\ntool_reminder = false\ntool_reminder_text = \"use as tools\"\n"
        );
        let cfg: Config = toml::from_str(&com_agente).unwrap();
        assert_eq!(cfg.agent.reminder_line(), None);
        assert_eq!(
            cfg.agent.tool_reminder_text.as_deref(),
            Some("use as tools")
        );
    }

    #[test]
    fn arquivo_largo_rejeitado() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("x.toml");
        std::fs::write(&p, BASE).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(load(&p).is_err());
    }
}
