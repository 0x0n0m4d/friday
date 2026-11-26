//! TUI: conversa rolável, caixa de entrada, status e modal de aprovação.
//! Todo texto que sai do modelo é sanitizado antes de render.

use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use crossterm::event::Event as CEvent;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::app::{App, AppEvent, Author, Status};
use crate::sanitize;

pub use key_parse::{key_matches, ParsedKey};

pub mod key_parse {
    //! Tradução de teclas para ações. A tecla de voz em modo "toggle"
    //! também é reconhecida aqui (o hold evdev não é teclado do terminal).

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[derive(Debug, Clone)]
    pub enum ParsedKey {
        Quit,
        Cancel,
        Submit,
        ScrollUp,
        ScrollDown,
        Backspace,
        InsertChar(char),
        Paste(String),
        ToggleKey,
        ModalUp,
        ModalDown,
        ModalSelect,
    }

    pub fn parse_key(ev: &KeyEvent, toggle_key: Option<&str>) -> Option<ParsedKey> {
        if ev.kind == crossterm::event::KeyEventKind::Release {
            return None;
        }
        if let Some(tk) = toggle_key {
            if key_matches(tk, ev) {
                return Some(ParsedKey::ToggleKey);
            }
        }
        match ev.code {
            KeyCode::Char('c') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ParsedKey::Quit)
            }
            KeyCode::Esc => Some(ParsedKey::Cancel),
            KeyCode::Enter => Some(ParsedKey::Submit),
            KeyCode::Backspace => Some(ParsedKey::Backspace),
            KeyCode::Up => Some(ParsedKey::ModalUp),
            KeyCode::Down => Some(ParsedKey::ModalDown),
            // Rolagem por Ctrl+U/Ctrl+D (semântica less/vim: meia página por
            // toque). Tem de vir ANTES do catch-all de InsertChar, senão o
            // 'u'/'d' vira texto na caixa de entrada com Ctrl apertado.
            KeyCode::Char('u') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ParsedKey::ScrollUp)
            }
            KeyCode::Char('d') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ParsedKey::ScrollDown)
            }
            KeyCode::Char(c) => Some(ParsedKey::InsertChar(c)),
            _ => None,
        }
    }

    /// Compara a tecla configurada (ex.: "f8", "ctrl+g") com o evento.
    pub fn key_matches(pattern: &str, ev: &KeyEvent) -> bool {
        let p = pattern.trim().to_lowercase();
        let (mods_wanted, code_part) = match p.strip_prefix("ctrl+") {
            Some(rest) => (KeyModifiers::CONTROL, rest.to_string()),
            None => (KeyModifiers::empty(), p),
        };
        if !mods_wanted.is_empty() && !ev.modifiers.contains(mods_wanted) {
            return false;
        }
        match code_part.as_str() {
            "f8" => ev.code == KeyCode::F(8),
            "f1" => ev.code == KeyCode::F(1),
            "f2" => ev.code == KeyCode::F(2),
            "scrolllock" => ev.code == KeyCode::ScrollLock,
            "pause" => ev.code == KeyCode::Pause,
            "esc" => ev.code == KeyCode::Esc,
            other if other.chars().count() == 1 => {
                let c = other.chars().next().unwrap_or('\0');
                ev.code == KeyCode::Char(c)
            }
            _ => false,
        }
    }
}

// ===================== linha principal da TUI =====================

pub fn run(mut app: App, events_rx: Receiver<AppEvent>) {
    let mut terminal = match TermGuard::prepare() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("falha ao iniciar o terminal: {e}");
            return;
        }
    };
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        previous_hook(info);
    }));

    let toggle_key = if app.cfg.push_to_talk.mode == "toggle" {
        Some(app.cfg.push_to_talk.fallback_key.clone())
    } else {
        None
    };
    let parsed_toggle = toggle_key.clone();
    let key_tx = app.events_tx.clone();
    let keyboard = std::thread::Builder::new()
        .name("keyboard".into())
        .spawn(move || keyboard_thread(key_tx, parsed_toggle.as_deref()));
    if let Err(e) = keyboard {
        app.info(format!(
            "teclado do terminal não iniciou: {e} — digitação indisponível"
        ));
    }

    app.start();
    let key_rx = events_rx;

    while !app.quit {
        // Drena as respostas do agente antes de esperar eventos: sem isto o
        // fio leitor enche o canal e o handshake apodrece (initialize nunca
        // é processado). Ver docs/DECISIONS.md.
        app.pump_acp(Duration::from_millis(0));
        match key_rx.recv_timeout(Duration::from_millis(25)) {
            Ok(ev) => handle_event(&mut app, ev),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                app.events_tx.send(AppEvent::Tick).ok();
                handle_event(&mut app, AppEvent::Tick);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // sem fontes de evento: sai
                break;
            }
        }
        if terminal.draw(|f| draw(&app, f)).is_err() {
            break;
        }
    }
    app.teardown();
    let _ = restore_terminal();
}

fn handle_event(app: &mut App, ev: AppEvent) {
    match ev {
        AppEvent::Key(pk) => match pk {
            ParsedKey::ToggleKey => {
                if toggle_now(app) {
                    app.on_key_press();
                } else {
                    app.on_key_release();
                }
            }
            other => app.on_key(other),
        },
        AppEvent::Acp(inbound) => app.on_acp(inbound),
        AppEvent::KeySource(crate::ptt::KeySourceEvent::Press) => app.on_key_press(),
        AppEvent::KeySource(crate::ptt::KeySourceEvent::Release) => app.on_key_release(),
        AppEvent::KeySource(crate::ptt::KeySourceEvent::Failed(msg)) => {
            app.on_key_source_failed(msg);
        }
        AppEvent::RecordDone(r) => app.on_record_done(r),
        AppEvent::Transcribed(t) => app.on_transcribed(t),
        AppEvent::TtsStarted => {
            if app.status != Status::Listening {
                app.status = Status::Speaking;
            }
        }
        AppEvent::TtsFinished(Err(e)) => {
            app.status = Status::Error(format!("voz: {e}"));
            app.log.log_error("erro_tts", &e.to_string());
        }
        AppEvent::TtsFinished(Ok(())) => {
            if app.status == Status::Speaking {
                app.status = Status::Idle;
            }
        }
        AppEvent::TtsInterrupted => {
            if app.status == Status::Speaking {
                app.status = Status::Idle;
            }
        }
        AppEvent::Tick => app.on_tick(),
    }
}

fn toggle_now(app: &App) -> bool {
    // "não ativo" → "ativo" quando não há gravação em andamento
    app.recording.is_none()
}

fn keyboard_thread(tx: Sender<AppEvent>, toggle_key: Option<&str>) {
    loop {
        match crossterm::event::read() {
            Ok(CEvent::Key(ev)) => {
                if let Some(pk) = key_parse::parse_key(&ev, toggle_key) {
                    if tx.send(AppEvent::Key(pk)).is_err() {
                        return;
                    }
                }
            }
            Ok(CEvent::Paste(text)) => {
                if tx.send(AppEvent::Key(ParsedKey::Paste(text))).is_err() {
                    return;
                }
            }
            Ok(CEvent::Resize(_, _)) => {
                let _ = tx.send(AppEvent::Tick);
            }
            Err(_) => return,
            _ => {}
        }
    }
}

fn restore_terminal() -> io::Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen
    )?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(())
}

// ===================== render =====================

struct TermGuard;

impl TermGuard {
    fn prepare() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste
        )?;
        let backend = CrosstermBackend::new(io::stdout());
        Terminal::new(backend)
    }
}

/// Desenha a interface inteira num quadro.
pub fn draw(app: &App, f: &mut Frame) {
    let areas = Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(f.area());

    draw_chat(app, f, areas[0]);
    draw_input(app, f, areas[1]);
    draw_status(app, f, areas[2]);
    if let Some(modal) = app
        .pending_permission
        .as_ref()
        .filter(|_| app.status == Status::Approval)
    {
        let modal_area = {
            let width = f.area().width.clamp(20, 60);
            let height = (modal.options.len() as u16 + 4).min(f.area().height);
            let x = (f.area().width.saturating_sub(width)) / 2;
            let y = (f.area().height.saturating_sub(height)) / 2;
            Rect::new(x, y, width, height)
        };
        draw_modal(app, f, modal_area);
    }
}

/// Fatiamento por COLUNAS de terminal: quebra `texto` em pedaços de largura
/// visual ≤ `largura` (emoji/largura-dupla contam certo via unicode-width).
// A causa do "texto sobressaindo" era o descasamento entre o offset calculado
// em linhas lógicas e o render do Paragraph, que rola linhas VISUAIS (com o
// wrap do widget). Fatiamos ourselves: 1 Line = 1 linha visível — o offset
// passa a ser exato, e a âncora no fundo nunca corta a última frase.
fn fatia_por_colunas(texto: &str, largura: usize) -> Vec<String> {
    let largura = largura.max(4); // janela mínima absurda: degrada, não trava
    let mut partes: Vec<String> = Vec::new();
    let mut atual = String::new();
    let mut usado = 0usize;
    for c in texto.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if usado + w > largura && !atual.is_empty() {
            partes.push(std::mem::take(&mut atual));
            usado = 0;
        }
        atual.push(c);
        usado += w;
    }
    if !atual.is_empty() || partes.is_empty() {
        partes.push(atual);
    }
    partes
}

fn draw_chat(app: &App, f: &mut Frame, area: Rect) {
    // -2: bordas do bloco; é este o wrap width efetivo do widget interno.
    let largura = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();
    for item in &app.chat {
        let label = match item.author {
            Author::User => "Você",
            Author::Agent => "Sexta-feira",
            Author::Info => "·",
            Author::Diag => "[diag]",
        };
        let color = match item.author {
            Author::User => Color::LightBlue,
            Author::Agent => Color::LightGreen,
            Author::Info => Color::DarkGray,
            Author::Diag => Color::Yellow,
        };
        let prefixo = Span::styled(
            format!("{label}: "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        );
        let texto = sanitize::strip_escapes_and_control(&item.rendered());
        for (i, bruta) in texto.lines().enumerate() {
            let (prefixo_linha, cols): (Option<Span>, usize) = if i == 0 {
                let w = unicode_width::UnicodeWidthStr::width(prefixo.content.as_ref());
                (Some(prefixo.clone()), w)
            } else {
                // continuação do mesmo item: alinhada sob o rótulo
                (None, 13)
            };
            let texto_padrao = Span::raw("             ");
            let disp = largura.saturating_sub(cols).max(4);
            for (j, pedaco) in fatia_por_colunas(bruta, disp).into_iter().enumerate() {
                let mut spans: Vec<Span> = Vec::new();
                if j == 0 {
                    if let Some(p) = &prefixo_linha {
                        spans.push(p.clone());
                    } else {
                        spans.push(texto_padrao.clone());
                    }
                }
                spans.push(Span::raw(pedaco));
                lines.push(Line::from(spans));
            }
        }
    }
    let total = lines.len();
    let visible = area.height.saturating_sub(2) as usize;
    app.viewport_rows.set(visible as u16);
    // âncora pelo FUNDO: novidade chegando não desloca uma tela que o usuário
    // rolou manualmente; com follow_tail o offset é exatamente a última linha
    let de_fundo = if app.follow_tail {
        0usize
    } else {
        app.scroll_offset
    };
    let scroll = total.saturating_sub(visible).saturating_sub(de_fundo);
    let chat = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Sexta-feira "),
        )
        .scroll((scroll as u16, 0));
    f.render_widget(chat, area);
}

fn draw_input(app: &App, f: &mut Frame, area: Rect) {
    let frozen = app.input_frozen_draft.is_some();
    let title = if frozen {
        " mensagem (rascunho guardado; tecla de voz ativa) "
    } else {
        " mensagem "
    };
    let input = Paragraph::new(app.input.as_str())
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    f.render_widget(input, area);
    if !frozen {
        let width = unicode_width::UnicodeWidthStr::width(app.input.as_str()) as u16;
        let x = area.x + 1 + width;
        let y = area.y + 1;
        f.set_cursor_position((x.min(area.x + area.width.saturating_sub(2)), y));
    }
}

/// Nome de tecla legível para o código evdev configurado (levantado do
/// /usr/include/linux/input-event-codes.h). Nada sai de caminho de dispositivo
/// nem número cru na tela: "[voz] alt-right" diz tudo que interessa.
fn nome_da_tecla_evdev(code: u16) -> String {
    let nome = match code {
        1 => "esc",
        14 => "backspace",
        15 => "tab",
        28 => "enter",
        29 => "ctrl-left",
        42 => "shift-left",
        54 => "shift-right",
        56 => "alt-left",
        57 => "espaco",
        58 => "caps-lock",
        69 => "num-lock",
        70 => "scroll-lock",
        87 => "f11",
        88 => "f12",
        97 => "ctrl-right",
        100 => "alt-right",
        102 => "home",
        104 => "pgup",
        105 => "seta-esq",
        106 => "seta-dir",
        107 => "end",
        108 => "seta-baixo",
        109 => "pgdn",
        110 => "insert",
        111 => "delete",
        119 => "pause",
        125 => "super-left",
        126 => "super-right",
        59..=68 => return format!("f{}", code - 58), // F1..F10 são sequenciais
        outro => return format!("tecla {outro}"),
    };
    nome.to_string()
}

fn draw_status(app: &App, f: &mut Frame, area: Rect) {
    let (status_text, status_color) = match &app.status {
        Status::Idle => ("Pronto", Color::Gray),
        Status::Listening => ("Ouvindo…", Color::Red),
        Status::Transcribing => ("Transcrevendo…", Color::LightYellow),
        Status::Waiting => ("Aguardando o agente…", Color::LightCyan),
        Status::Speaking => ("Falando…", Color::LightMagenta),
        Status::Disconnected => ("Desconectado — reconectando…", Color::Red),
        Status::Approval => ("Aprovação necessária (teclado)", Color::LightRed),
        Status::Error(e) => (e.as_str(), Color::Red),
    };
    // ESQUERDA: só o estado. Nada mais disputa a atenção ali.
    let chip = format!(" {status_text} ");
    let largura_chip = unicode_width::UnicodeWidthStr::width(chip.as_str())
        .min(area.width.saturating_sub(1) as usize) as u16;
    let areas = Layout::default()
        .direction(ratatui::layout::Direction::Horizontal)
        .constraints([Constraint::Length(largura_chip), Constraint::Min(0)])
        .split(area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            chip,
            Style::default().fg(Color::Black).bg(status_color),
        ))),
        areas[0],
    );
    // DIREITA: o mínimo que não é óbvio — voz, cancelamento e rolagem.
    // Fora: Enter envia (óbvio), Ctrl+C (óbvio num terminal), dispositivo
    // e código cru da tecla de voz (ruído).
    let voz = if app.cfg.push_to_talk.mode == "hold" {
        app.cfg
            .push_to_talk
            .code
            .map(|c| format!("[voz] {} · ", nome_da_tecla_evdev(c)))
            .unwrap_or_default()
    } else {
        format!("[voz] alterna {} · ", app.cfg.push_to_talk.fallback_key)
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("{voz}Esc cancela · Ctrl+U/Ctrl+D rola "),
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Right),
        areas[1],
    );
}

fn draw_modal(app: &App, f: &mut Frame, area: Rect) {
    let Some(modal) = &app.pending_permission else {
        return;
    };
    f.render_widget(Clear, area);
    let mut text: Vec<Line> = Vec::new();
    text.push(Line::from(Span::styled(
        format!(
            "Aprovação: {}",
            sanitize::strip_escapes_and_control(&modal.question)
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for (i, (_, name)) in modal.options.iter().enumerate() {
        let marker = if i == modal.selected { "› " } else { "  " };
        let style = if i == modal.selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        text.push(Line::from(Span::styled(
            format!("{marker}{}", sanitize::strip_escapes_and_control(name)),
            style,
        )));
    }
    text.push(Line::from(Span::styled(
        "↑/↓ escolhe · Enter responde · Esc cancela (nada é aprovado por voz)",
        Style::default().fg(Color::DarkGray),
    )));
    let p = Paragraph::new(text)
        .block(Block::default().borders(Borders::ALL).title(" Aprovação "))
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatia_respeita_largura_e_nao_perde_caractere() {
        let pedacos = fatia_por_colunas("abcdefghij", 4);
        assert_eq!(pedacos, vec!["abcd", "efgh", "ij"]);
        // recomposição: nada se perde nem se duplica
        assert_eq!(pedacos.concat(), "abcdefghij");
    }

    #[test]
    fn largura_dupla_emoji_nao_estoura_a_coluna() {
        // 🎉 vale 2 colunas: 3 emojis não cabem em largura 5
        let pedacos = fatia_por_colunas("🎉🎉🎉", 5);
        assert_eq!(pedacos.len(), 2);
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(pedacos[0].as_str()),
            4
        );
    }

    #[test]
    fn linha_vazia_gera_uma_parte_unica() {
        assert_eq!(fatia_por_colunas("", 10), vec![String::new()]);
        assert_eq!(fatia_por_colunas("ok", 10), vec!["ok"]);
    }

    #[test]
    fn tecla_do_dispositivo_vira_nome_legivel() {
        // 100 = KEY_RIGHTALT (KEY_LEFTALT é 56): o relato em campo
        assert_eq!(nome_da_tecla_evdev(100), "alt-right");
        assert_eq!(nome_da_tecla_evdev(57), "espaco");
        // F1..F10 são sequenciais a partir de 59
        assert_eq!(nome_da_tecla_evdev(59), "f1");
        assert_eq!(nome_da_tecla_evdev(68), "f10");
        // código desconhecido degrada para identificável, nunca some
        assert_eq!(nome_da_tecla_evdev(999), "tecla 999");
    }

    #[test]
    fn ctrl_rolagem_nao_vira_texto_na_entrada() {
        let ev = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('u'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert!(matches!(
            key_parse::parse_key(&ev, None),
            Some(ParsedKey::ScrollUp)
        ));
        // sem Ctrl, o 'u' continua digitando
        let ev_solto = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('u'),
            crossterm::event::KeyModifiers::empty(),
        );
        assert!(matches!(
            key_parse::parse_key(&ev_solto, None),
            Some(ParsedKey::InsertChar('u'))
        ));
    }
}
