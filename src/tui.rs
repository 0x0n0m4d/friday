//! TUI: conversa rolável, caixa de entrada, status e modal de aprovação.
//! Todo texto que sai do modelo é sanitizado antes de render.

use std::io;
use std::io::Write;
use std::process::Command;
use std::sync::mpsc::{self, Receiver};
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
        ToggleAutoSend,
        OpenEditor,
        Backspace,
        Delete,
        CursorLeft,
        CursorRight,
        Home,
        End,
        KillWord,
        KillToEnd,
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
            // Edição padrão da caixa: setas andam o cursor, Home/End saltam,
            // Delete apaga À FRENTE (Backspace apaga atrás).
            KeyCode::Left => Some(ParsedKey::CursorLeft),
            KeyCode::Right => Some(ParsedKey::CursorRight),
            KeyCode::Home => Some(ParsedKey::Home),
            KeyCode::End => Some(ParsedKey::End),
            KeyCode::Delete => Some(ParsedKey::Delete),
            // Edição readline: Ctrl+W apaga a palavra à esquerda, Ctrl+K
            // mata até o fim da linha (Ctrl+A aqui é auto-send, não Home).
            KeyCode::Char('w') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ParsedKey::KillWord)
            }
            KeyCode::Char('k') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ParsedKey::KillToEnd)
            }
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
            // Ctrl+A alterna o envio automático da voz; Ctrl+G abre o rascunho
            // no editor externo. Antes do catch-all de InsertChar, senão o
            // 'a'/'g' vira texto digitado com Ctrl apertado.
            KeyCode::Char('a') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ParsedKey::ToggleAutoSend)
            }
            KeyCode::Char('g') if ev.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ParsedKey::OpenEditor)
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

    app.start();
    let key_rx = events_rx;

    // O teclado do terminal é lido NESTE fio, com poll de 0ms. É isto que
    // dá o controle total ao editor externo (Ctrl+G): quando ele abre, o
    // MESMO fio que lia o teclado fica dentro do editor até ele fechar —
    // não existe nenhum outro leitor disputando a entrada, então as teclas
    // do editor (neovim, kakoune, o que for) NUNCA vazam para a caixa de
    // entrada do cliente. (Antes havia um fio separado em `event::read()`
    // eterno: enquanto o editor rodava, ele continuava sugando o teclado.)
    let mut teclado_vivo = true;

    while !app.quit {
        // Drena as respostas do agente antes de esperar eventos: sem isto o
        // fio leitor enche o canal e o handshake apodrece (initialize nunca
        // é processado). Ver docs/DECISIONS.md.
        app.pump_acp(Duration::from_millis(0));
        if teclado_vivo {
            while crossterm::event::poll(Duration::from_millis(0)).unwrap_or(false) {
                match crossterm::event::read() {
                    Ok(CEvent::Key(ev)) => {
                        if let Some(pk) = key_parse::parse_key(&ev, toggle_key.as_deref()) {
                            // Ctrl+G é outro fluxo: o editor roda com o terminal
                            // do USUÁRIO (tela cheia), nunca dentro do painel —
                            // a TUI desce, o editor abre, e tudo volta quando
                            // ele sai. Aqui ninguém mais lê o teclado até lá.
                            if matches!(pk, ParsedKey::OpenEditor) {
                                abrir_editor(&mut app, &mut terminal);
                            } else {
                                handle_event(&mut app, AppEvent::Key(pk));
                            }
                            if app.quit {
                                break;
                            }
                        }
                    }
                    Ok(CEvent::Paste(text)) => {
                        handle_event(&mut app, AppEvent::Key(ParsedKey::Paste(text)));
                    }
                    Ok(CEvent::Resize(_, _)) => {
                        // nada: o draw do fim do laço já redesenha tudo
                    }
                    Err(_) => {
                        // Fonte de eventos morta: reporta UMA vez e para de
                        // ler (o laço abaixo continuaria drenando erro).
                        teclado_vivo = false;
                        app.on_key_source_failed(
                            "leitura do teclado do terminal falhou; digitação indisponível".into(),
                        );
                        break;
                    }
                    _ => {}
                }
            }
        }
        if app.quit {
            break;
        }
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

/// Mecânica do Ctrl+G, sem terminal (testável): escreve o rascunho num
/// arquivo temporário (0600), roda o editor — argv PURO, sem shell, com o
/// caminho como último argumento — e devolve o conteúdo do arquivo depois
/// que o editor fecha. Mesma mecânica de `git commit`.
pub fn editar_texto(entrada: &str, argv: &[String]) -> io::Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let Some(prog) = argv.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "comando de editor vazio",
        ));
    };
    let caminho = std::env::temp_dir().join(format!(
        "sexta-editor-{}-{}.md",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&caminho)?
        .write_all(entrada.as_bytes())?;
    let status = Command::new(prog).args(&argv[1..]).arg(&caminho).status();
    let resultado = match status {
        Ok(s) if s.success() => std::fs::read_to_string(&caminho),
        Ok(s) => Err(io::Error::other(format!("editor saiu com {s}"))),
        Err(e) => Err(io::Error::other(format!(
            "editor não executou ({prog}): {e}"
        ))),
    };
    let _ = std::fs::remove_file(&caminho);
    resultado
}

/// Ctrl+G completo: desce a TUI (raw mode/tela alternada são do editor agora),
/// roda o editor sobre o rascunho e remarca o terminal. Falha do editor deixa
/// o rascunho INTACTO (nada se perde silenciosamente).
fn abrir_editor(app: &mut App, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    let Some(argv) = app.comando_editor() else {
        app.diag(
            "editor não configurado: defina [ui].editor no config, ou $VISUAL/$EDITOR no shell"
                .into(),
        );
        return;
    };
    let original = app.input.clone();
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen);
    let resultado = editar_texto(&original, &argv);
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen);
    if crossterm::terminal::enable_raw_mode().is_ok() {
        // quadro inteiro é refeito no próximo draw; limpar evita restos do
        // editor misturados ao painel
        let _ = terminal.clear();
    }
    match resultado {
        Ok(novo) => app.definir_input_do_editor(novo),
        Err(e) => {
            app.input = original;
            app.info(format!("editor: {e}"));
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
        // O cursor fica onde o usuário DEIXOU (input_cursor em chars), não
        // colado no fim: a largura exibida é a soma das colunas dos chars
        // ANTES do cursor (emoji/acentuados ocupam mais de 1 coluna).
        let colunas_antes: usize = app
            .input
            .chars()
            .take(app.input_cursor)
            .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
            .sum();
        let x = area.x + 1 + colunas_antes as u16;
        let y = area.y + 1;
        f.set_cursor_position((x.min(area.x + area.width.saturating_sub(2)), y));
    }
}

/// Ícones da barra de status (Nerd Font): ESTÁTICOS aqui de propósito — é
/// só trocar o codepoint nestas constantes para trocar o glifo na barra.
const ICONE_MIC: &str = "";
const ICONE_VOL: &str = "󰤌";

fn draw_status(app: &App, f: &mut Frame, area: Rect) {
    let (status_text, status_color) = match &app.status {
        Status::Idle => ("Pronto", Color::Green),
        Status::Listening => ("Ouvindo…", Color::Blue),
        Status::Transcribing => ("Transcrevendo…", Color::Yellow),
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
    // DIREITA: placeholder de help (estático; sem recurso implementado por
    // decisão do usuário) + o microfone. O glifo denuncia o modo de envio
    // (cortado = auto-send OFF) e a cor denuncia a escuta: cinza parado,
    // vermelho OUINDO — mesma cor do chip, impossível não notar.
    let (icone, cor) = if app.auto_send {
        (ICONE_MIC, Color::DarkGray)
    } else {
        (ICONE_VOL, Color::DarkGray)
    };
    let cor = if app.status == Status::Listening {
        Color::Blue
    } else {
        cor
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("[H] help  ·  ", Style::default().fg(Color::DarkGray)),
            Span::styled(icone, Style::default().fg(cor)),
            Span::raw("  "),
        ]))
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
    fn ctrl_a_e_ctrl_g_sao_acoes_nao_texto() {
        let com_ctrl = |c: char| {
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::CONTROL,
            )
        };
        assert!(matches!(
            key_parse::parse_key(&com_ctrl('a'), None),
            Some(ParsedKey::ToggleAutoSend)
        ));
        assert!(matches!(
            key_parse::parse_key(&com_ctrl('g'), None),
            Some(ParsedKey::OpenEditor)
        ));
        // sem Ctrl, 'a' e 'g' continuam digitando normalmente
        let solto_q = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('q'),
            crossterm::event::KeyModifiers::empty(),
        );
        assert!(matches!(
            key_parse::parse_key(&solto_q, None),
            Some(ParsedKey::InsertChar('q'))
        ));
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

    #[test]
    fn editor_externo_edita_o_rascunho_de_verdade() {
        // argv puro com um editor de verdade da máquina: tudo por stdout/env
        // da config, sem shell e sem comando fixo no produto (o 'sed' é do
        // TESTE, não do cliente).
        let argv: Vec<String> = vec!["sed".into(), "-i".into(), "s/rascunho/EDITADO/".into()];
        let saida = editar_texto("linha com rascunho aqui", &argv).expect("editor deveria rodar");
        assert!(saida.contains("EDITADO"), "conteúdo: {saida:?}");
        assert!(!saida.contains("rascunho"), "conteúdo: {saida:?}");
    }

    #[test]
    fn editor_que_falha_nao_silencia() {
        let argv = vec!["false".to_string()];
        let erro = editar_texto("qualquer", &argv).expect_err("false sai com erro");
        assert!(erro.to_string().contains("saiu com"));
    }

    #[test]
    fn comando_de_editor_vazio_rejeita() {
        let erro = editar_texto("x", &[]).expect_err("argv vazio");
        assert!(erro.to_string().contains("vazio"));
    }
}
