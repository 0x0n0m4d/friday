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
            KeyCode::PageUp => Some(ParsedKey::ScrollUp),
            KeyCode::PageDown => Some(ParsedKey::ScrollDown),
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

fn draw_chat(app: &App, f: &mut Frame, area: Rect) {
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
        let text = item.rendered();
        for (i, part) in text.lines().enumerate() {
            let mut spans: Vec<Span> = Vec::new();
            if i == 0 {
                spans.push(Span::styled(
                    format!("{label}: "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::raw("             "));
            }
            spans.push(Span::raw(sanitize::strip_escapes_and_control(part)));
            lines.push(Line::from(spans));
        }
    }
    let total = lines.len();
    let visible = area.height.saturating_sub(2) as usize;
    let scroll = if app.follow_tail {
        total.saturating_sub(visible) as u16
    } else {
        app.scroll_offset as u16
    };
    let chat = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Sexta-feira "),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
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
    let ptt_hint = match app.cfg.push_to_talk.mode.as_str() {
        "hold" => match (
            app.cfg.push_to_talk.device.clone(),
            app.cfg.push_to_talk.code,
        ) {
            (Some(d), Some(c)) => format!(" · [voz] {d} tecla {c}"),
            _ => String::new(),
        },
        _ => format!(" · [voz] alterna {}", app.cfg.push_to_talk.fallback_key),
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {status_text} "),
            Style::default().fg(Color::Black).bg(status_color),
        ),
        Span::styled(ptt_hint, Style::default().fg(Color::DarkGray)),
        Span::raw(" ─ Esc cancela · Enter envia · PgUp/PgDn rola · Ctrl+C sai"),
    ]);
    f.render_widget(Paragraph::new(line), area);
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
