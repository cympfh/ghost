//! Terminal UI: command line, streaming agent pane, input, and status bar.

use std::io::{self, IsTerminal, Stdout};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr;

use crate::github::{ItemKind, OpenItem};
use crate::lang::Lang;
use crate::text::{pad_cols, truncate_cols, wrap_cols};
use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Review,
    Code,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub kind: LogKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Agent,
    System,
    User,
    Error,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub mode: String,
    pub target: String,
    pub phase: String,
    pub sha: String,
    pub ci: String,
    pub turns: String,
    pub agent: String,
    pub session: String,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            mode: "starting".into(),
            target: "—".into(),
            phase: "starting".into(),
            sha: "—".into(),
            ci: "—".into(),
            turns: "0".into(),
            agent: "—".into(),
            session: "—".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum View {
    Log,
    Picker {
        items: Vec<OpenItem>,
        selected: usize,
    },
    Choose {
        number: u64,
        title: String,
    },
    Confirm {
        number: u64,
        message: String,
        kind: ConfirmKind,
        lang: Lang,
    },
    Fatal {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    Log(LogLine),
    Status(Status),
    Picker(Vec<OpenItem>),
    /// Open issues only. The empty list does not mention pull requests.
    IssuePicker(Vec<OpenItem>),
    ChooseMode {
        number: u64,
        title: String,
    },
    Confirm {
        number: u64,
        message: String,
        kind: ConfirmKind,
        lang: Lang,
    },
    Fatal(String),
    Exit {
        code: i32,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCmd {
    Quit,
    Interrupt,
    Input(String),
    Pick {
        kind: ItemKind,
        number: u64,
        side: Option<Side>,
    },
    Choose(Side),
    Confirm {
        answer: ConfirmAnswer,
        /// Method selected on the merge dialog. Other prompts leave this empty.
        method: Option<MergeStyle>,
    },
}

/// `gh pr merge` strategy shown on the merge dialog. `m`, Tab, and the arrow
/// keys walk squash → merge → rebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStyle {
    Squash,
    Merge,
    Rebase,
}

impl MergeStyle {
    pub fn cycle(self) -> Self {
        match self {
            Self::Squash => Self::Merge,
            Self::Merge => Self::Rebase,
            Self::Rebase => Self::Squash,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Squash => "squash",
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        }
    }

    pub fn flag(self) -> &'static str {
        match self {
            Self::Squash => "--squash",
            Self::Merge => "--merge",
            Self::Rebase => "--rebase",
        }
    }

    /// `--merge-method`, or squash when the flag was omitted.
    pub fn from_flag(method: Option<&str>) -> Self {
        match crate::github::merge_method_flag(method) {
            "--merge" => Self::Merge,
            "--rebase" => Self::Rebase,
            _ => Self::Squash,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    /// Issue handoff. Enter is yes.
    ContinueAsCode,
    /// Merge prompt. Enter, Esc, and Ctrl-C are no. `m` changes [`MergeStyle`].
    Merge(MergeStyle),
    /// Approve verdict. Enter, Esc, and Ctrl-C exit. No merge.
    ReviewExit,
}

/// Question line for the merge dialog. The footer is [`confirm_footer`].
pub fn merge_question(number: u64, method: MergeStyle, lang: Lang) -> String {
    match lang {
        Lang::Ja => format!("PR #{number} を merge しますか？ 方法: {}", method.word()),
        Lang::En => format!("Merge PR #{number}? method: {}", method.word()),
    }
}

/// Keys and the Enter default, derived from the prompt kind. Capital letter is Enter.
pub fn confirm_footer(kind: ConfirmKind, lang: Lang) -> String {
    match (kind, lang) {
        (ConfirmKind::Merge(method), Lang::Ja) => format!(
            "y merge / N(Enter) やめる / c 続ける / m 方法: {}",
            method.word()
        ),
        (ConfirmKind::Merge(method), Lang::En) => format!(
            "y merge / N(Enter) cancel / c continue / m method: {}",
            method.word()
        ),
        (ConfirmKind::ReviewExit, Lang::Ja) => "Y(Enter) 終了 / c 続ける".into(),
        (ConfirmKind::ReviewExit, Lang::En) => "Y(Enter) exit / c continue".into(),
        (ConfirmKind::ContinueAsCode, Lang::Ja) => "Y(Enter) 続ける / n やめる".into(),
        (ConfirmKind::ContinueAsCode, Lang::En) => "Y(Enter) yes / n no".into(),
    }
}

fn confirm_title(kind: ConfirmKind, lang: Lang) -> &'static str {
    match (kind, lang) {
        (ConfirmKind::Merge(_), _) => " merge ",
        (ConfirmKind::ReviewExit, Lang::Ja) => " 終了 ",
        (ConfirmKind::ReviewExit, Lang::En) => " exit ",
        (ConfirmKind::ContinueAsCode, Lang::Ja) => " 続ける ",
        (ConfirmKind::ContinueAsCode, Lang::En) => " continue ",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAnswer {
    Yes,
    No,
    Continue,
}

pub struct App {
    pub header: String,
    pub lines: Vec<LogLine>,
    pub stick: bool,
    pub scroll: usize,
    pub input: String,
    pub cursor: usize,
    pub input_mode: bool,
    pub status: Status,
    pub view: View,
    pub quit: bool,
    pub exit_code: i32,
    pub exit_message: String,
    pub spinner: usize,
    picker_backup: Vec<OpenItem>,
    picker_selected: usize,
    issues_only: bool,
}

impl App {
    pub fn new(header: String) -> Self {
        Self {
            header,
            lines: Vec::new(),
            stick: true,
            scroll: 0,
            input: String::new(),
            cursor: 0,
            input_mode: false,
            status: Status::default(),
            view: View::Log,
            quit: false,
            exit_code: 0,
            exit_message: String::new(),
            spinner: 0,
            picker_backup: Vec::new(),
            picker_selected: 0,
            issues_only: false,
        }
    }

    fn push_log(&mut self, line: LogLine) {
        for part in line.text.split('\n') {
            let text = part.trim_end_matches('\r').to_string();
            if text.is_empty() {
                continue;
            }
            self.lines.push(LogLine {
                kind: line.kind,
                text,
            });
        }
        if self.lines.len() > 2000 {
            let drop_n = self.lines.len() - 2000;
            self.lines.drain(0..drop_n);
        }
    }
}

/// True only when both streams are already terminals. crossterm opens
/// `/dev/tty` when stdin is not a terminal, and that open fails with ENXIO
/// (os error 6) when the process has no controlling terminal.
pub fn should_open_terminal(stdin_tty: bool, stdout_tty: bool) -> bool {
    stdin_tty && stdout_tty
}

pub fn interactive_terminal() -> bool {
    should_open_terminal(io::stdin().is_terminal(), io::stdout().is_terminal())
}

pub fn run<F>(header: String, worker: F) -> anyhow::Result<i32>
where
    F: FnOnce(Sender<UiEvent>, Receiver<UiCmd>) + Send + 'static,
{
    if !interactive_terminal() {
        return run_headless(header, worker);
    }
    let (ev_tx, ev_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let join = thread::spawn(move || worker(ev_tx, cmd_rx));

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut app = App::new(header);

    crate::agent::install_fatal_signals();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        crate::agent::kill_tracked_agent();
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        hook(info);
    }));

    loop {
        terminal.draw(|f| draw(f, &mut app))?;
        while let Ok(event) = ev_rx.try_recv() {
            apply(&mut app, event);
        }
        if app.quit {
            break;
        }
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                if let Some(cmd) = on_key(&mut app, key) {
                    let _ = cmd_tx.send(cmd);
                }
            }
        }
        app.spinner = app.spinner.wrapping_add(1);
    }

    restore(&mut terminal)?;
    drop(cmd_tx);
    let _ = join.join();
    if !app.exit_message.is_empty() {
        println!("{}", app.exit_message);
    }
    Ok(app.exit_code)
}

/// No `/dev/tty`. Logs go to stdout. Confirms are answered by the engine
/// before they are sent; a confirm that still arrives quits without merging.
fn run_headless<F>(header: String, worker: F) -> anyhow::Result<i32>
where
    F: FnOnce(Sender<UiEvent>, Receiver<UiCmd>) + Send + 'static,
{
    let (ev_tx, ev_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let join = thread::spawn(move || worker(ev_tx, cmd_rx));
    if !header.is_empty() {
        println!("{header}");
    }
    let mut exit_code = 0;
    let mut exit_message = String::new();
    let mut forced = false;
    loop {
        match ev_rx.recv() {
            Ok(UiEvent::Log(line)) => {
                if !line.text.is_empty() {
                    println!("{}", line.text);
                }
            }
            Ok(UiEvent::Status(_)) => {}
            Ok(UiEvent::Picker(_))
            | Ok(UiEvent::IssuePicker(_))
            | Ok(UiEvent::ChooseMode { .. }) => {
                forced = true;
                exit_code = 1;
                exit_message = "not a TTY; pass a pull request or issue URL".into();
                eprintln!("ghost: {exit_message}");
                let _ = cmd_tx.send(UiCmd::Quit);
            }
            Ok(UiEvent::Confirm { message, .. }) => {
                println!("{message}");
                forced = true;
                exit_code = 0;
                exit_message = "not a TTY; not waiting for a key".into();
                println!("ghost: {exit_message}");
                let _ = cmd_tx.send(UiCmd::Quit);
            }
            Ok(UiEvent::Fatal(message)) => {
                eprintln!("{message}");
                exit_code = 1;
                exit_message = message;
                // Unblock a worker waiting on the command channel (preflight).
                let _ = cmd_tx.send(UiCmd::Quit);
            }
            Ok(UiEvent::Exit { code, message }) => {
                if !forced {
                    exit_code = code;
                    if !message.is_empty() {
                        exit_message = message;
                    }
                }
                break;
            }
            Err(_) => break,
        }
    }
    drop(cmd_tx);
    let _ = join.join();
    if !exit_message.is_empty() {
        println!("{exit_message}");
    }
    Ok(exit_code)
}

fn restore(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn apply(app: &mut App, event: UiEvent) {
    match event {
        UiEvent::Log(line) => app.push_log(line),
        UiEvent::Status(status) => app.status = status,
        UiEvent::Picker(items) => show_picker(app, items, false),
        UiEvent::IssuePicker(items) => show_picker(app, items, true),
        UiEvent::ChooseMode { number, title } => {
            app.view = View::Choose { number, title };
            app.input_mode = false;
        }
        UiEvent::Confirm {
            number,
            message,
            kind,
            lang,
        } => {
            app.view = View::Confirm {
                number,
                message,
                kind,
                lang,
            };
            app.input_mode = false;
        }
        UiEvent::Fatal(message) => {
            app.exit_code = 1;
            app.exit_message = message.clone();
            app.view = View::Fatal { message };
            app.input_mode = false;
        }
        UiEvent::Exit { code, message } => {
            app.exit_code = code;
            if !message.is_empty() {
                app.exit_message = message;
            }
            app.quit = true;
        }
    }
}

pub fn on_key(app: &mut App, key: KeyEvent) -> Option<UiCmd> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        if app.input_mode {
            app.input_mode = false;
            return None;
        }
        if let View::Confirm { kind, .. } = app.view {
            return Some(match kind {
                ConfirmKind::Merge(method) => UiCmd::Confirm {
                    answer: ConfirmAnswer::No,
                    method: Some(method),
                },
                ConfirmKind::ReviewExit => UiCmd::Confirm {
                    answer: ConfirmAnswer::Yes,
                    method: None,
                },
                ConfirmKind::ContinueAsCode => UiCmd::Confirm {
                    answer: ConfirmAnswer::No,
                    method: None,
                },
            });
        }
        return Some(UiCmd::Interrupt);
    }
    if app.input_mode {
        return on_input_key(app, key);
    }
    if key.code == KeyCode::Char('q') {
        app.quit = true;
        if app.exit_message.is_empty() {
            app.exit_message = "quit".into();
        }
        return Some(UiCmd::Quit);
    }

    let kind = match &app.view {
        View::Fatal { .. } => ViewKind::Fatal,
        View::Confirm { .. } => ViewKind::Confirm,
        View::Choose { .. } => ViewKind::Choose,
        View::Picker { .. } => ViewKind::Picker,
        View::Log => ViewKind::Log,
    };
    match (kind, key.code) {
        (ViewKind::Fatal, KeyCode::Enter) => {
            app.quit = true;
            Some(UiCmd::Quit)
        }
        (ViewKind::Confirm, KeyCode::Char('y') | KeyCode::Char('Y')) => {
            let method = merge_style(&app.view);
            app.view = View::Log;
            Some(UiCmd::Confirm {
                answer: ConfirmAnswer::Yes,
                method,
            })
        }
        (ViewKind::Confirm, KeyCode::Char('c') | KeyCode::Char('C')) => {
            if matches!(
                app.view,
                View::Confirm {
                    kind: ConfirmKind::Merge(_) | ConfirmKind::ReviewExit,
                    ..
                }
            ) {
                app.view = View::Log;
                Some(UiCmd::Confirm {
                    answer: ConfirmAnswer::Continue,
                    method: None,
                })
            } else {
                None
            }
        }
        (
            ViewKind::Confirm,
            KeyCode::Char('m') | KeyCode::Char('M') | KeyCode::Tab | KeyCode::Left | KeyCode::Right,
        ) => {
            cycle_merge_method(app);
            None
        }
        (ViewKind::Confirm, KeyCode::Enter) => confirm_enter(app),
        (ViewKind::Confirm, KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc) => {
            confirm_decline(app)
        }
        (ViewKind::Choose, KeyCode::Char('r')) => {
            app.view = View::Log;
            Some(UiCmd::Choose(Side::Review))
        }
        (ViewKind::Choose, KeyCode::Char('c')) => {
            app.view = View::Log;
            Some(UiCmd::Choose(Side::Code))
        }
        (ViewKind::Choose, KeyCode::Esc) => {
            if app.picker_backup.is_empty() {
                None
            } else {
                app.view = View::Picker {
                    items: app.picker_backup.clone(),
                    selected: app.picker_selected,
                };
                None
            }
        }
        (ViewKind::Picker, KeyCode::Char('j') | KeyCode::Down) => move_picker(app, 1),
        (ViewKind::Picker, KeyCode::Char('k') | KeyCode::Up) => move_picker(app, -1),
        (ViewKind::Picker, KeyCode::Char('r')) => picker_action(app, Some(Side::Review)),
        (ViewKind::Picker, KeyCode::Char('c')) => picker_action(app, Some(Side::Code)),
        (ViewKind::Picker, KeyCode::Enter) => picker_enter(app),
        (ViewKind::Log, KeyCode::Char('i')) => {
            app.input_mode = true;
            None
        }
        (ViewKind::Log, KeyCode::Char('j') | KeyCode::Down) => {
            app.stick = false;
            app.scroll = app.scroll.saturating_add(1);
            None
        }
        (ViewKind::Log, KeyCode::Char('k') | KeyCode::Up) => {
            app.stick = false;
            app.scroll = app.scroll.saturating_sub(1);
            None
        }
        (ViewKind::Log, KeyCode::Char('g') | KeyCode::Home) => {
            app.stick = false;
            app.scroll = 0;
            None
        }
        (ViewKind::Log, KeyCode::Char('G') | KeyCode::End) => {
            app.stick = true;
            None
        }
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum ViewKind {
    Log,
    Picker,
    Choose,
    Confirm,
    Fatal,
}

fn move_picker(app: &mut App, delta: i32) -> Option<UiCmd> {
    let View::Picker { items, selected } = &app.view else {
        return None;
    };
    let len = items.len();
    let items = items.clone();
    let next = if delta > 0 {
        (*selected + 1).min(len.saturating_sub(1))
    } else {
        selected.saturating_sub(1)
    };
    app.picker_selected = next;
    app.view = View::Picker {
        items,
        selected: next,
    };
    None
}

fn show_picker(app: &mut App, items: Vec<OpenItem>, issues_only: bool) {
    app.issues_only = issues_only;
    app.picker_backup = items.clone();
    app.picker_selected = 0;
    app.view = View::Picker { items, selected: 0 };
    app.input_mode = false;
}

fn picker_action(app: &mut App, side: Option<Side>) -> Option<UiCmd> {
    let View::Picker { items, selected } = &app.view else {
        return None;
    };
    let item = items.get(*selected)?.clone();
    if item.kind == ItemKind::Issue && side.is_some() {
        return None;
    }
    app.view = View::Log;
    Some(UiCmd::Pick {
        kind: item.kind,
        number: item.number,
        side,
    })
}

fn picker_enter(app: &mut App) -> Option<UiCmd> {
    let View::Picker { items, selected } = &app.view else {
        return None;
    };
    let item = items.get(*selected)?.clone();
    let selected = *selected;
    match item.kind {
        ItemKind::Issue => {
            app.view = View::Log;
            Some(UiCmd::Pick {
                kind: ItemKind::Issue,
                number: item.number,
                side: None,
            })
        }
        ItemKind::Pull => {
            app.picker_selected = selected;
            app.view = View::Choose {
                number: item.number,
                title: item.title,
            };
            None
        }
    }
}

fn on_input_key(app: &mut App, key: KeyEvent) -> Option<UiCmd> {
    match key.code {
        KeyCode::Esc => {
            app.input_mode = false;
            None
        }
        KeyCode::Enter => {
            let text = app.input.clone();
            app.input.clear();
            app.cursor = 0;
            app.input_mode = false;
            Some(UiCmd::Input(text))
        }
        KeyCode::Backspace => {
            if app.cursor > 0 {
                let end = char_byte(&app.input, app.cursor);
                let start = char_byte(&app.input, app.cursor - 1);
                app.input.replace_range(start..end, "");
                app.cursor -= 1;
            }
            None
        }
        KeyCode::Left => {
            app.cursor = app.cursor.saturating_sub(1);
            None
        }
        KeyCode::Right => {
            if app.cursor < app.input.chars().count() {
                app.cursor += 1;
            }
            None
        }
        KeyCode::Char(c) => {
            let idx = char_byte(&app.input, app.cursor);
            app.input.insert(idx, c);
            app.cursor += 1;
            None
        }
        _ => None,
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 40 || area.height < 10 {
        frame.render_widget(Paragraph::new("terminal too small"), area);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    frame.render_widget(header_line(&app.header), chunks[0]);
    draw_body(frame, chunks[1], app);
    frame.render_widget(input_line(app, chunks[2].width as usize), chunks[2]);
    frame.render_widget(status_line(app, chunks[3].width as usize), chunks[3]);
    frame.render_widget(hint_line(app), chunks[4]);

    if let View::Choose { number, title } = &app.view {
        draw_choose(frame, chunks[1], *number, title);
    }
    if let View::Confirm {
        message,
        kind,
        lang,
        ..
    } = &app.view
    {
        draw_confirm(frame, chunks[1], message, *kind, *lang);
    }
}

fn header_line(header: &str) -> Paragraph<'_> {
    Paragraph::new(Line::from(vec![
        Span::styled("$ ", theme::prompt()),
        Span::styled(header.to_string(), theme::title()),
    ]))
}

fn border_style(app: &App) -> ratatui::style::Style {
    if matches!(app.view, View::Fatal { .. }) {
        theme::border_fatal()
    } else if app.input_mode {
        theme::border_input()
    } else if app.status.phase == "running" {
        theme::border_running()
    } else if matches!(app.view, View::Confirm { .. }) {
        theme::border_confirm()
    } else {
        theme::border_idle()
    }
}

fn draw_body(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style(app));
    match &app.view {
        View::Fatal { message } => {
            let para = Paragraph::new(message.clone())
                .style(theme::text())
                .block(block)
                .wrap(Wrap { trim: false });
            frame.render_widget(para, area);
        }
        View::Picker { items, selected } => {
            let width = area.width.saturating_sub(2) as usize;
            let issues_only = app.issues_only;
            let (rows, selected_row) = picker_rows(items, *selected, width, issues_only);
            let inner_h = area.height.saturating_sub(2) as usize;
            let offset = selected_row.saturating_sub(inner_h.saturating_sub(1));
            let para = Paragraph::new(rows).block(block).scroll((offset as u16, 0));
            frame.render_widget(para, area);
        }
        _ => {
            let width = area.width.saturating_sub(2).saturating_sub(2) as usize;
            let width = width.max(1);
            let rows = log_rows(&app.lines, width);
            let inner_h = area.height.saturating_sub(2) as usize;
            let max_scroll = rows.len().saturating_sub(inner_h);
            if app.stick || app.scroll > max_scroll {
                app.scroll = max_scroll;
            }
            let para = Paragraph::new(rows)
                .block(block)
                .scroll((app.scroll as u16, 0));
            frame.render_widget(para, area);
        }
    }
}

fn log_rows(lines: &[LogLine], width: usize) -> Vec<Line<'static>> {
    if lines.is_empty() {
        return vec![Line::from(Span::styled("· …", theme::secondary()))];
    }
    let mut rows = Vec::new();
    for line in lines {
        let (prefix, style) = match line.kind {
            LogKind::Agent => ("> ", theme::text()),
            LogKind::System => ("· ", theme::secondary()),
            LogKind::User => ("› ", theme::warn()),
            LogKind::Error => ("! ", theme::error()),
        };
        let prefix_style = match line.kind {
            LogKind::Agent => theme::accent_bold(),
            LogKind::User => theme::warn(),
            LogKind::Error => theme::error(),
            LogKind::System => theme::secondary(),
        };
        let wrapped = wrap_cols(&line.text, width.max(1));
        for (i, part) in wrapped.into_iter().enumerate() {
            if i == 0 {
                rows.push(Line::from(vec![
                    Span::styled(prefix.to_string(), prefix_style),
                    Span::styled(part, style),
                ]));
            } else {
                rows.push(Line::from(Span::styled(format!("  {part}"), style)));
            }
        }
    }
    rows
}

fn picker_rows(
    items: &[OpenItem],
    selected: usize,
    width: usize,
    issues_only: bool,
) -> (Vec<Line<'static>>, usize) {
    if items.is_empty() {
        let text = if issues_only {
            "  no open issues"
        } else {
            "  no open pull requests or issues"
        };
        return (vec![Line::from(Span::styled(text, theme::secondary()))], 0);
    }
    let mut rows = Vec::new();
    let mut selected_row = 0;
    let mut saw_pr = false;
    let mut saw_issue = false;
    for (idx, item) in items.iter().enumerate() {
        if item.kind == ItemKind::Pull && !saw_pr {
            rows.push(section("PULL REQUESTS"));
            saw_pr = true;
        }
        if item.kind == ItemKind::Issue && !saw_issue {
            rows.push(section("ISSUES"));
            saw_issue = true;
        }
        if idx == selected {
            selected_row = rows.len();
        }
        rows.push(item_line(item, idx == selected, width));
    }
    (rows, selected_row)
}

fn section(label: &str) -> Line<'static> {
    Line::from(Span::styled(format!(" {label}"), theme::accent_bold()))
}

fn item_line(item: &OpenItem, selected: bool, width: usize) -> Line<'static> {
    let width = width.max(20);
    let marker = if selected { "▸" } else { " " };
    let num = format!("#{}", item.number);
    let author = if item.author.is_empty() {
        String::new()
    } else {
        format!("@{}", item.author)
    };
    let labels = item
        .labels
        .iter()
        .take(2)
        .cloned()
        .collect::<Vec<_>>()
        .join(",");
    let ci = item.ci.clone().unwrap_or_default();
    let review = item.review.clone().unwrap_or_default();
    let tail = format!("{author} {labels} {ci} {review}");
    let tail_w = UnicodeWidthStr::width(tail.as_str()) + 1;
    let prefix = format!("{marker} {num} ");
    let prefix_w = UnicodeWidthStr::width(prefix.as_str());
    let title_w = width.saturating_sub(prefix_w + tail_w).max(8);
    let title = pad_cols(&item.title, title_w);
    let plain = format!("{prefix}{title} {tail}");
    if selected {
        return Line::from(Span::styled(
            truncate_cols(&plain, width),
            theme::selected(),
        ));
    }
    Line::from(vec![
        Span::styled(prefix, theme::text()),
        Span::styled(title, theme::text()),
        Span::raw(" "),
        Span::styled(author, theme::secondary()),
        Span::raw(" "),
        Span::styled(labels, theme::accent()),
        Span::raw(" "),
        Span::styled(ci, theme::badge(item.ci.as_deref().unwrap_or(""))),
        Span::raw(" "),
        Span::styled(review, theme::badge(item.review.as_deref().unwrap_or(""))),
    ])
}

fn input_line(app: &App, width: usize) -> Paragraph<'static> {
    if app.input_mode {
        let avail = width.saturating_sub(4).max(1);
        let (shown, cursor) = window_str(&app.input, app.cursor, avail);
        let mut spans = vec![Span::styled(" ❯ ", theme::warn_bold())];
        let chars: Vec<char> = shown.chars().collect();
        for (i, ch) in chars.iter().enumerate() {
            if i == cursor {
                spans.push(Span::styled(ch.to_string(), theme::cursor()));
            } else {
                spans.push(Span::styled(ch.to_string(), theme::text()));
            }
        }
        if cursor >= chars.len() {
            spans.push(Span::styled(" ", theme::cursor_block()));
        }
        Paragraph::new(Line::from(spans))
    } else {
        Paragraph::new(Line::from(vec![
            Span::styled(" ❯ ", theme::warn()),
            Span::styled(idle_hint(&app.view), theme::secondary()),
        ]))
    }
}

fn char_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

fn window_str(text: &str, cursor: usize, width: usize) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    if chars.len() <= width {
        return (chars.into_iter().collect(), cursor);
    }
    let start = cursor.saturating_sub(width / 2);
    let start = start.min(chars.len().saturating_sub(width));
    let end = (start + width).min(chars.len());
    let shown: String = chars[start..end].iter().collect();
    (shown, cursor - start)
}

fn merge_style(view: &View) -> Option<MergeStyle> {
    match view {
        View::Confirm {
            kind: ConfirmKind::Merge(style),
            ..
        } => Some(*style),
        _ => None,
    }
}

fn cycle_merge_method(app: &mut App) {
    let View::Confirm {
        kind: ConfirmKind::Merge(style),
        message,
        number,
        lang,
        ..
    } = &mut app.view
    else {
        return;
    };
    *style = style.cycle();
    *message = merge_question(*number, *style, *lang);
}

fn confirm_enter(app: &mut App) -> Option<UiCmd> {
    let (answer, method) = match app.view {
        View::Confirm {
            kind: ConfirmKind::Merge(style),
            ..
        } => (ConfirmAnswer::No, Some(style)),
        View::Confirm {
            kind: ConfirmKind::ReviewExit | ConfirmKind::ContinueAsCode,
            ..
        } => (ConfirmAnswer::Yes, None),
        _ => return None,
    };
    app.view = View::Log;
    Some(UiCmd::Confirm { answer, method })
}

fn confirm_decline(app: &mut App) -> Option<UiCmd> {
    let (answer, method) = match app.view {
        View::Confirm {
            kind: ConfirmKind::ReviewExit,
            ..
        } => (ConfirmAnswer::Yes, None),
        View::Confirm {
            kind: ConfirmKind::Merge(style),
            ..
        } => (ConfirmAnswer::No, Some(style)),
        View::Confirm {
            kind: ConfirmKind::ContinueAsCode,
            ..
        } => (ConfirmAnswer::No, None),
        _ => return None,
    };
    app.view = View::Log;
    Some(UiCmd::Confirm { answer, method })
}

fn idle_hint(view: &View) -> String {
    match view {
        View::Picker { .. } => "r review · c code · enter select".into(),
        View::Choose { .. } => "r review · c code · esc back".into(),
        View::Confirm { kind, lang, .. } => confirm_footer(*kind, *lang),
        View::Fatal { .. } => "q quit".into(),
        View::Log => "i message the agent".into(),
    }
}

fn status_line(app: &App, width: usize) -> Paragraph<'static> {
    let spin = if app.status.phase == "running" {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[app.spinner % FRAMES.len()]
    } else {
        " "
    };
    let text = format!(
        " {spin} {}  {}  {}  sha {}  CI {}  {}  {}  {}",
        app.status.mode,
        app.status.target,
        app.status.phase,
        app.status.sha,
        app.status.ci,
        app.status.turns,
        app.status.agent,
        app.status.session,
    );
    Paragraph::new(Line::from(Span::styled(
        pad_cols(&text, width.max(1)),
        theme::status_bar(),
    )))
}

fn hint_line(app: &App) -> Paragraph<'static> {
    let text = if app.input_mode {
        " enter send · esc cancel · backspace delete".to_string()
    } else {
        match &app.view {
            View::Picker { .. } => " j/k move · enter select · r review · c code · q quit".into(),
            View::Choose { .. } => " r review · c code · esc back · q quit".into(),
            View::Confirm { kind, lang, .. } => confirm_footer(*kind, *lang),
            View::Fatal { .. } => " q quit".into(),
            View::Log => {
                " q quit · i input · j/k scroll · ctrl-c interrupt · g/G top/bottom".into()
            }
        }
    };
    Paragraph::new(Line::from(Span::styled(text, theme::secondary())))
}

fn draw_choose(frame: &mut Frame, area: Rect, number: u64, title: &str) {
    let rect = centered(area, 48.min(area.width), 9.min(area.height));
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_choose())
        .title(" mode ");
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(format!(" PR #{number}"), theme::title())),
        Line::from(Span::styled(
            format!(
                " {}",
                truncate_cols(title, rect.width.saturating_sub(4) as usize)
            ),
            theme::secondary(),
        )),
        Line::from(""),
        Line::from(Span::styled(" r  review", theme::review_choice())),
        Line::from(Span::styled(" c  code as the author", theme::code_choice())),
    ])
    .block(block);
    frame.render_widget(body, rect);
}

fn draw_confirm(frame: &mut Frame, area: Rect, message: &str, kind: ConfirmKind, lang: Lang) {
    let width = 74.min(area.width);
    let height = 9.min(area.height);
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let footer = confirm_footer(kind, lang);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme::border_confirm())
        .title(confirm_title(kind, lang));
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(Span::styled(message.to_string(), theme::title())),
        Line::from(""),
        Line::from(Span::styled(footer, theme::warn())),
    ])
    .block(block)
    .wrap(Wrap { trim: false });
    frame.render_widget(body, rect);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;

    #[test]
    fn non_tty_does_not_open_the_terminal() {
        assert!(!super::should_open_terminal(false, false));
        assert!(
            !super::should_open_terminal(false, true),
            "stdin is not a tty; opening /dev/tty is what returns os error 6"
        );
        assert!(!super::should_open_terminal(true, false));
        assert!(super::should_open_terminal(true, true));
    }

    #[test]
    fn headless_exits_without_opening_a_terminal() {
        let code = super::run_headless("ghost".into(), |tx, _rx| {
            let _ = tx.send(UiEvent::Log(LogLine {
                kind: LogKind::System,
                text: "headless ok".into(),
            }));
            let _ = tx.send(UiEvent::Exit {
                code: 0,
                message: "done".into(),
            });
        })
        .unwrap();
        assert_eq!(code, 0);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            let mut x = 0u16;
            while x < buf.area.width {
                let sym = buf[(x, y)].symbol();
                let width = unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
                s.push_str(sym);
                x = x.saturating_add(width);
            }
            s.push('\n');
        }
        s
    }

    fn confirm(number: u64, message: &str, kind: ConfirmKind, lang: Lang) -> View {
        View::Confirm {
            number,
            message: message.into(),
            kind,
            lang,
        }
    }

    #[test]
    fn confirm_defaults_match_each_prompt() {
        let mut app = App::new("ghost github.com/acme/app/issues/7".into());
        app.view = confirm(
            12,
            "Continue as --code on PR #12?",
            ConfirmKind::ContinueAsCode,
            Lang::En,
        );
        assert_eq!(
            on_key(&mut app, key(KeyCode::Enter)),
            Some(UiCmd::Confirm {
                answer: ConfirmAnswer::Yes,
                method: None,
            })
        );
        app.view = confirm(
            12,
            "Continue as --code on PR #12?",
            ConfirmKind::ContinueAsCode,
            Lang::En,
        );
        assert_eq!(
            on_key(&mut app, key(KeyCode::Char('n'))),
            Some(UiCmd::Confirm {
                answer: ConfirmAnswer::No,
                method: None,
            })
        );

        app.view = confirm(
            3,
            &merge_question(3, MergeStyle::Squash, Lang::Ja),
            ConfirmKind::Merge(MergeStyle::Squash),
            Lang::Ja,
        );
        assert_eq!(on_key(&mut app, key(KeyCode::Char('m'))), None);
        assert_eq!(on_key(&mut app, key(KeyCode::Tab)), None);
        match &app.view {
            View::Confirm {
                kind: ConfirmKind::Merge(MergeStyle::Rebase),
                message,
                ..
            } => {
                assert!(message.contains("方法: rebase"), "{message}");
            }
            other => panic!("expected rebase, {other:?}"),
        }
        assert_eq!(
            on_key(&mut app, key(KeyCode::Char('y'))),
            Some(UiCmd::Confirm {
                answer: ConfirmAnswer::Yes,
                method: Some(MergeStyle::Rebase),
            })
        );

        app.view = confirm(
            3,
            &merge_question(3, MergeStyle::Squash, Lang::Ja),
            ConfirmKind::Merge(MergeStyle::Squash),
            Lang::Ja,
        );
        assert_eq!(
            on_key(&mut app, key(KeyCode::Enter)),
            Some(UiCmd::Confirm {
                answer: ConfirmAnswer::No,
                method: Some(MergeStyle::Squash),
            })
        );
        app.view = confirm(
            3,
            &merge_question(3, MergeStyle::Merge, Lang::Ja),
            ConfirmKind::Merge(MergeStyle::Merge),
            Lang::Ja,
        );
        assert_eq!(
            on_key(&mut app, key(KeyCode::Esc)),
            Some(UiCmd::Confirm {
                answer: ConfirmAnswer::No,
                method: Some(MergeStyle::Merge),
            })
        );
        app.view = confirm(
            3,
            &merge_question(3, MergeStyle::Squash, Lang::Ja),
            ConfirmKind::Merge(MergeStyle::Squash),
            Lang::Ja,
        );
        assert_eq!(
            on_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ),
            Some(UiCmd::Confirm {
                answer: ConfirmAnswer::No,
                method: Some(MergeStyle::Squash),
            }),
            "ctrl-c on the merge confirm must not merge"
        );
        app.view = confirm(
            3,
            &merge_question(3, MergeStyle::Squash, Lang::Ja),
            ConfirmKind::Merge(MergeStyle::Squash),
            Lang::Ja,
        );
        assert_eq!(
            on_key(&mut app, key(KeyCode::Char('c'))),
            Some(UiCmd::Confirm {
                answer: ConfirmAnswer::Continue,
                method: None,
            })
        );
        app.view = confirm(
            12,
            "Continue as --code on PR #12?",
            ConfirmKind::ContinueAsCode,
            Lang::En,
        );
        assert_eq!(
            on_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ),
            Some(UiCmd::Confirm {
                answer: ConfirmAnswer::No,
                method: None,
            })
        );

        app.view = confirm(
            3,
            "PR #3 は問題なしと判断しました。終了しますか？",
            ConfirmKind::ReviewExit,
            Lang::Ja,
        );
        assert_eq!(
            on_key(&mut app, key(KeyCode::Enter)),
            Some(UiCmd::Confirm {
                answer: ConfirmAnswer::Yes,
                method: None,
            })
        );
        app.view = confirm(
            3,
            "PR #3 は問題なしと判断しました。終了しますか？",
            ConfirmKind::ReviewExit,
            Lang::Ja,
        );
        assert_eq!(
            on_key(&mut app, key(KeyCode::Char('c'))),
            Some(UiCmd::Confirm {
                answer: ConfirmAnswer::Continue,
                method: None,
            })
        );
    }

    #[test]
    fn merge_dialog_renders_enter_as_no_and_the_method() {
        let mut app = App::new("ghost github.com/acme/app/pull/3 --code".into());
        app.view = confirm(
            3,
            &merge_question(3, MergeStyle::Squash, Lang::Ja),
            ConfirmKind::Merge(MergeStyle::Squash),
            Lang::Ja,
        );
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let text = buffer_text(term.backend().buffer());
        assert!(
            !text.contains("[Y]"),
            "merge dialog must not use the generic yes bracket\n{text}"
        );
        assert!(
            !text.to_ascii_lowercase().contains("accepts the default"),
            "merge dialog must not say enter accepts\n{text}"
        );
        assert!(text.contains("N(Enter)"), "{text}");
        assert!(text.contains("やめる"), "{text}");
        assert!(text.contains("方法: squash"), "{text}");
        assert!(text.contains("y merge"), "{text}");
        assert!(!text.contains("continue?"), "{text}");

        app.view = confirm(
            3,
            &merge_question(3, MergeStyle::Squash, Lang::En),
            ConfirmKind::Merge(MergeStyle::Squash),
            Lang::En,
        );
        term.draw(|f| draw(f, &mut app)).unwrap();
        let en = buffer_text(term.backend().buffer());
        assert!(!en.contains("[Y]"), "{en}");
        assert!(
            !en.to_ascii_lowercase().contains("accepts the default"),
            "{en}"
        );
        assert!(en.contains("N(Enter)"), "{en}");
        assert!(en.contains("m method: squash"), "{en}");

        app.view = confirm(
            3,
            "PR #3 は問題なしと判断しました。終了しますか？",
            ConfirmKind::ReviewExit,
            Lang::Ja,
        );
        term.draw(|f| draw(f, &mut app)).unwrap();
        let review = buffer_text(term.backend().buffer());
        assert!(review.contains("Y(Enter) 終了 / c 続ける"), "{review}");
        assert!(
            !review.to_ascii_lowercase().contains("accepts the default"),
            "{review}"
        );

        app.view = confirm(
            12,
            "Continue as --code on PR #12?",
            ConfirmKind::ContinueAsCode,
            Lang::En,
        );
        term.draw(|f| draw(f, &mut app)).unwrap();
        let handoff = buffer_text(term.backend().buffer());
        assert!(handoff.contains("Y(Enter) yes / n no"), "{handoff}");
        assert!(
            !handoff.to_ascii_lowercase().contains("accepts the default"),
            "{handoff}"
        );
    }

    #[test]
    fn picker_moves_and_selects() {
        let items = vec![
            OpenItem {
                kind: ItemKind::Pull,
                number: 3,
                title: "Add ghost".into(),
                author: "ada".into(),
                labels: vec!["tui".into()],
                ci: Some("CI …".into()),
                review: Some("review".into()),
            },
            OpenItem {
                kind: ItemKind::Issue,
                number: 7,
                title: "Crash".into(),
                author: "mei".into(),
                labels: vec![],
                ci: None,
                review: None,
            },
        ];
        let mut app = App::new("ghost github.com/acme/app".into());
        app.view = View::Picker {
            items: items.clone(),
            selected: 0,
        };
        assert!(on_key(&mut app, key(KeyCode::Char('j'))).is_none());
        match &app.view {
            View::Picker { selected, .. } => assert_eq!(*selected, 1),
            other => panic!("unexpected {other:?}"),
        }
        let cmd = on_key(&mut app, key(KeyCode::Enter)).unwrap();
        assert_eq!(
            cmd,
            UiCmd::Pick {
                kind: ItemKind::Issue,
                number: 7,
                side: None,
            }
        );
    }

    #[test]
    fn input_edits_by_character() {
        let mut app = App::new("ghost".into());
        app.input_mode = true;
        for ch in "あい".chars() {
            on_input_key(&mut app, key(KeyCode::Char(ch)));
        }
        on_input_key(&mut app, key(KeyCode::Left));
        on_input_key(&mut app, key(KeyCode::Char('う')));
        on_input_key(&mut app, key(KeyCode::Backspace));
        assert_eq!(app.input, "あい");
        assert_eq!(app.cursor, 1);
    }

    #[test]
    fn review_frame_shows_agent_stream() {
        let mut app = App::new("ghost github.com/cympfh/xxx/pull/3 --review".into());
        app.status = Status {
            mode: "review".into(),
            target: "PR #3".into(),
            phase: "running".into(),
            sha: "4f1c2ab".into(),
            ci: "pending".into(),
            turns: "1/20".into(),
            agent: "claude".into(),
            session: "7f3ac2e1".into(),
        };
        app.lines.push(LogLine {
            kind: LogKind::Agent,
            text:
                "こんにちは。該当のrepositoryが手元にあるかみて、PR#3をなんかしますうんぬん。。。"
                    .into(),
        });
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let text = buffer_text(term.backend().buffer());
        assert!(text.contains("こんにちは"), "{text}");
        assert!(text.contains("PR #3"), "{text}");
        assert!(
            text.contains("running") || text.contains("review"),
            "{text}"
        );
    }

    #[test]
    fn secondary_text_is_gray_and_no_color_is_plain() {
        use ratatui::style::{Color, Modifier};

        let mut app = App::new("ghost github.com/cympfh/xxx/pull/3 --review".into());
        app.lines.push(LogLine {
            kind: LogKind::System,
            text: "reviews 1, inline 1, comments 0".into(),
        });
        app.lines.push(LogLine {
            kind: LogKind::System,
            text: "filtered 0".into(),
        });

        theme::force_colors_for_test(Some(true));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut saw_gray = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                assert_ne!(cell.fg, Color::DarkGray);
                assert!(!cell.modifier.contains(Modifier::DIM));
                if cell.fg == Color::Gray {
                    saw_gray = true;
                }
            }
        }
        assert!(saw_gray, "system and hint text use gray");

        theme::force_colors_for_test(Some(false));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                assert_eq!(cell.fg, Color::Reset);
                assert_eq!(cell.bg, Color::Reset);
                assert!(!cell.modifier.contains(Modifier::DIM));
            }
        }
        theme::force_colors_for_test(None);
    }
}
