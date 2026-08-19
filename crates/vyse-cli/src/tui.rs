use std::io::{self, stdout};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::store::LoggedRequest;

pub fn run_tui(
    public_url: String,
    rx: mpsc::Receiver<LoggedRequest>,
    quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    update_notice: Option<Arc<Mutex<Option<String>>>>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = event_loop(&mut terminal, public_url, rx, quit, update_notice);
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    public_url: String,
    rx: mpsc::Receiver<LoggedRequest>,
    quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    update_notice: Option<Arc<Mutex<Option<String>>>>,
) -> Result<()> {
    let mut items: Vec<LoggedRequest> = Vec::new();
    let mut footer = "Replay later with: vyse replay <id>".to_string();
    loop {
        if quit.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        while let Ok(row) = rx.try_recv() {
            items.insert(0, row);
            items.truncate(50);
        }
        if let Some(notice) = update_notice.as_ref()
            && let Ok(guard) = notice.lock()
            && let Some(message) = guard.as_ref()
        {
            footer = format!("Replay later with: vyse replay <id>  ·  vyse update");
            let _ = message;
        }
        terminal.draw(|frame| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(4),
                    Constraint::Length(2),
                ])
                .split(frame.area());
            frame.render_widget(
                Paragraph::new(format!("Vyse  {public_url}"))
                    .block(Block::default().borders(Borders::ALL).title("tunnel")),
                chunks[0],
            );
            let list_items: Vec<ListItem> = items
                .iter()
                .map(|row| {
                    let status = row
                        .status
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "-".into());
                    ListItem::new(format!(
                        "{}  {} {}  :{}  {}",
                        row.id, row.method, row.path, row.port, status
                    ))
                })
                .collect();
            frame.render_widget(
                List::new(list_items).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("webhooks (last 50) — q / Ctrl+C quit"),
                ),
                chunks[1],
            );
            frame.render_widget(
                Paragraph::new(footer.as_str()),
                chunks[2],
            );
        })?;
        if event::poll(Duration::from_millis(120))?
            && let Event::Key(key) = event::read()?
        {
            let quit_key = matches!(
                key.code,
                KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('Q')
            ) || (key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL));
            if quit_key {
                quit.store(true, std::sync::atomic::Ordering::Relaxed);
                break;
            }
        }
    }
    Ok(())
}
