// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Terminal monitor using the same history, ETS and operation model as the GUI.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use crossterm::ExecutableCommand as _;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::interface::{self, Follower, MonitorModel};

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
    }
}

fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let old = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = io::stdout().execute(LeaveAlternateScreen);
            old(info);
        }));
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    Filter,
    Read,
    WriteAddress,
    WriteDpt,
    WriteValue,
    ConfirmWrite,
    Export,
}

struct App {
    model: MonitorModel,
    follower: Follower,
    action_receiver: Option<Receiver<Result<devknx::ipc::IpcMessage, String>>>,
    discovery_receiver: Option<Receiver<Result<Vec<knx_rs_ip::discovery::GatewayInfo>, String>>>,
    mode: Mode,
    input: String,
    write_address: String,
    write_dpt: String,
    write_value: String,
    preview: String,
    selected: usize,
    follow_tail: bool,
}

impl App {
    fn new(database: PathBuf) -> Result<Self, String> {
        let model = MonitorModel::open(database.clone())?;
        let selected = model.rows.len().saturating_sub(1);
        Ok(Self {
            model,
            follower: Follower::start(database),
            action_receiver: None,
            discovery_receiver: None,
            mode: Mode::Normal,
            input: String::new(),
            write_address: String::new(),
            write_dpt: String::new(),
            write_value: String::new(),
            preview: String::new(),
            selected,
            follow_tail: true,
        })
    }

    fn poll(&mut self) {
        for result in self.follower.receiver.try_iter().take(500) {
            match result {
                Ok(message) => self.model.ingest(message),
                Err(error) => self.model.owner_unavailable(&error),
            }
        }
        if self.follow_tail {
            self.selected = self.visible_count().saturating_sub(1);
        }
        if let Some(receiver) = &self.action_receiver {
            match receiver.try_recv() {
                Ok(Ok(message)) => {
                    self.model.ingest(message);
                    self.action_receiver = None;
                }
                Ok(Err(error)) => {
                    self.model.notice(error);
                    self.action_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.model
                        .notice("Operation worker stopped unexpectedly".into());
                    self.action_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(receiver) = &self.discovery_receiver {
            match receiver.try_recv() {
                Ok(Ok(gateways)) => {
                    let names = gateways
                        .iter()
                        .map(|gateway| gateway.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.model
                        .notice(format!("Discovered {} gateways: {names}", gateways.len()));
                    self.discovery_receiver = None;
                }
                Ok(Err(error)) => {
                    self.model.notice(error);
                    self.discovery_receiver = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.model
                        .notice("Gateway discovery stopped unexpectedly".into());
                    self.discovery_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    fn visible_count(&self) -> usize {
        self.model
            .rows
            .iter()
            .filter(|row| row.matches(&self.model.filter))
            .count()
    }

    fn start_operation(&mut self, request: devknx::operations::OperationRequest) {
        if self.action_receiver.is_some() {
            self.model.notice("An operation is already running".into());
            return;
        }
        let database = self.model.database.clone();
        let (sender, receiver) = mpsc::channel();
        self.action_receiver = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(interface::operate(&database, &request));
        });
    }

    fn submit(&mut self) {
        let input = std::mem::take(&mut self.input);
        match self.mode {
            Mode::Filter => {
                self.model.filter = input;
                self.selected = self.visible_count().saturating_sub(1);
                self.mode = Mode::Normal;
            }
            Mode::Read => {
                match interface::read_request(&input) {
                    Ok(request) => self.start_operation(request),
                    Err(error) => self.model.notice(error),
                }
                self.mode = Mode::Normal;
            }
            Mode::WriteAddress => {
                self.write_address = input;
                self.mode = Mode::WriteDpt;
            }
            Mode::WriteDpt => {
                self.write_dpt = input;
                self.mode = Mode::WriteValue;
            }
            Mode::WriteValue => {
                self.write_value = input;
                match self.model.preview_write(
                    &self.write_address,
                    &self.write_dpt,
                    &self.write_value,
                ) {
                    Ok(preview) => {
                        self.preview = preview;
                        self.mode = Mode::ConfirmWrite;
                    }
                    Err(error) => {
                        self.model.notice(error);
                        self.mode = Mode::Normal;
                    }
                }
            }
            Mode::Export => {
                match interface::export_csv(&self.model.database, &PathBuf::from(input.trim())) {
                    Ok(count) => self.model.notice(format!("Exported {count} captures")),
                    Err(error) => self.model.notice(error),
                }
                self.mode = Mode::Normal;
            }
            Mode::Normal | Mode::ConfirmWrite => {}
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the terminal key map is a single explicit state machine"
    )]
    fn key(&mut self, key: crossterm::event::KeyEvent) -> bool {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if key.code == KeyCode::Esc {
            self.mode = Mode::Normal;
            self.input.clear();
            return false;
        }
        if self.mode == Mode::ConfirmWrite {
            if key.code == KeyCode::Char('y') {
                match interface::write_request(
                    &self.write_address,
                    &self.write_dpt,
                    &self.write_value,
                ) {
                    Ok(request) => self.start_operation(request),
                    Err(error) => self.model.notice(error),
                }
            }
            self.mode = Mode::Normal;
            self.preview.clear();
            return false;
        }
        if self.mode != Mode::Normal {
            match key.code {
                KeyCode::Enter => self.submit(),
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input.push(ch);
                }
                _ => {}
            }
            return false;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
                self.input = self.model.filter.clone();
            }
            KeyCode::Char('r') => {
                self.mode = Mode::Read;
                self.input.clear();
            }
            KeyCode::Char('w') => {
                self.mode = Mode::WriteAddress;
                self.input.clear();
            }
            KeyCode::Char('e') => {
                self.mode = Mode::Export;
                self.input.clear();
            }
            KeyCode::Char('h') => {
                if let Err(error) = self.model.reload_history() {
                    self.model.notice(error);
                }
            }
            KeyCode::PageUp => {
                let previous_visible = self.visible_count();
                match self.model.load_older() {
                    Ok(0) => self.model.notice("No older retained captures".into()),
                    Ok(count) => {
                        let added_visible = self.visible_count().saturating_sub(previous_visible);
                        self.selected = self
                            .selected
                            .saturating_add(added_visible)
                            .min(self.visible_count().saturating_sub(1));
                        self.follow_tail = false;
                        self.model.notice(format!("Loaded {count} older captures"));
                    }
                    Err(error) => self.model.notice(error),
                }
            }
            KeyCode::Char('d') => {
                let (sender, receiver) = mpsc::channel();
                std::thread::spawn(move || {
                    let _ = sender.send(interface::discover_gateways());
                });
                self.discovery_receiver = Some(receiver);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.visible_count().saturating_sub(1));
                self.follow_tail = self.selected + 1 >= self.visible_count();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.follow_tail = false;
            }
            KeyCode::End => {
                self.selected = self.visible_count().saturating_sub(1);
                self.follow_tail = true;
            }
            KeyCode::Home => {
                self.selected = 0;
                self.follow_tail = false;
            }
            _ => {}
        }
        false
    }

    fn draw<B: Backend>(&self, terminal: &mut Terminal<B>) -> Result<(), B::Error> {
        terminal.draw(|frame| {
            let areas = Layout::vertical([Constraint::Length(3), Constraint::Min(5), Constraint::Length(3), Constraint::Length(3)])
                .split(frame.area());
            let title = format!(" devknx · {:?} · {} ", self.model.state, self.model.database.display());
            frame.render_widget(Paragraph::new(title).block(Block::default().borders(Borders::ALL)), areas[0]);

            let visible: Vec<_> = self.model.rows.iter().filter(|row| row.matches(&self.model.filter)).collect();
            let capacity = usize::from(areas[1].height.saturating_sub(2));
            let selected = self.selected.min(visible.len().saturating_sub(1));
            let start = selected.saturating_sub(capacity.saturating_sub(1));
            let end = (start + capacity).min(visible.len());
            let items: Vec<ListItem> = visible[start..end].iter().enumerate().map(|(index, row)| {
                let text = format!("{} {:8} {:8} → {:9} {:16} {}",
                    row.timestamp_ms, row.direction, row.source, row.destination,
                    row.service, row.label.as_deref().unwrap_or(""));
                let style = if start + index == selected { Style::default().fg(Color::Black).bg(Color::Cyan) }
                    else { Style::default() };
                ListItem::new(text).style(style)
            }).collect();
            frame.render_widget(List::new(items).block(Block::default().title(format!(
                " Captures · {} visible / {} buffered · filter: {} ",
                visible.len(), self.model.rows.len(), self.model.filter)).borders(Borders::ALL)), areas[1]);
            let detail = visible.get(selected).map_or_else(
                || "No capture selected".to_owned(),
                |row| format!("{} · {} · DPT [{}] · raw cEMI {}", row.destination,
                    row.label.as_deref().unwrap_or("no ETS label"), row.dpts.join(", "), row.raw_cemi));
            frame.render_widget(Paragraph::new(detail).block(Block::default().title(" Details ").borders(Borders::ALL)), areas[2]);
            let prompt = match self.mode {
                Mode::Normal => "q quit · / filter · r read · w write · e export · h reload · PgUp older · d discover · j/k scroll".to_owned(),
                Mode::Filter => format!("Filter: {}", self.input),
                Mode::Read => format!("Read group address: {}", self.input),
                Mode::WriteAddress => format!("Write group address: {}", self.input),
                Mode::WriteDpt => format!("DPT (blank = ETS): {}", self.input),
                Mode::WriteValue => format!("Typed value: {}", self.input),
                Mode::ConfirmWrite => format!("{} · y transmit / n or Esc cancel", self.preview),
                Mode::Export => format!("New CSV file path: {}", self.input),
            };
            let notice = self.model.notices.last().map_or("", String::as_str);
            frame.render_widget(Paragraph::new(format!("{prompt}\n{notice}"))
                .block(Block::default().borders(Borders::ALL)), areas[3]);
        })?;
        Ok(())
    }
}

pub fn run(database: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(database).map_err(io::Error::other)?;
    install_panic_hook();
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    loop {
        app.poll();
        app.draw(&mut terminal)?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && app.key(key)
        {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use devknx::storage::CaptureStore;
    use std::num::NonZeroU32;

    #[test]
    fn startup_resize_and_input_modes_render_without_a_live_owner() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("capture.sqlite");
        drop(CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap());
        let mut app = App::new(database).unwrap();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        app.draw(&mut terminal).unwrap();
        terminal.resize(Rect::new(0, 0, 40, 12)).unwrap();
        app.draw(&mut terminal).unwrap();
        assert!(!app.key(crossterm::event::KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.mode, Mode::Filter);
        app.input = "light".into();
        app.submit();
        assert_eq!(app.model.filter, "light");
        app.draw(&mut terminal).unwrap();
    }
}
