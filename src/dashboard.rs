//! Ratatui terminal dashboard: read-only view of the brain's health plus a
//! live recall pane. One screen, no daemon required — everything read
//! directly from local state and the index.

use std::io::Write as _;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::config::Config;
use crate::{daemon, exhaust, heartbeat, index, ledger, paths};

struct Stats {
    daemon_version: Option<String>,
    hooks: Vec<(String, u32, Option<u64>)>, // name, consecutive_failures, last_ok
    sessions: usize,
    injected_tokens: u64,
    quarantines: usize,
    docs_by_ring: Vec<(u8, i64)>,
    blocks: i64,
    code_files: i64,
    symbols: i64,
    staging_total: i64,
    staging_by_reason: Vec<(String, i64)>,
    exhaust_events: i64,
}

fn gather(conn: &rusqlite::Connection) -> Stats {
    let state = paths::state_dir();
    let hb = heartbeat::load_from(&state);
    let l = ledger::load();
    let docs_by_ring = conn
        .prepare("SELECT ring, count(*) FROM docs GROUP BY ring ORDER BY ring")
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get::<_, i64>(0)? as u8, r.get::<_, i64>(1)?)))
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    let one = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    // Read-only, fail-silent: an absent exhaust DB simply reports zeros.
    let staging = exhaust::stats(&state);
    Stats {
        daemon_version: daemon::call("ping", Duration::from_millis(200)).and_then(|r| r.version),
        hooks: hb
            .hooks
            .into_iter()
            .map(|(n, h)| (n, h.consecutive_failures, h.last_ok))
            .collect(),
        sessions: l.sessions.len(),
        injected_tokens: l
            .sessions
            .values()
            .flat_map(|s| s.by_source.values())
            .map(|t| t.tokens_estimated)
            .sum(),
        quarantines: ledger::quarantine_count(&state),
        docs_by_ring,
        blocks: one("SELECT count(*) FROM blocks"),
        code_files: one("SELECT count(*) FROM code_files"),
        symbols: one("SELECT count(*) FROM symbols"),
        staging_total: staging.staged_total,
        staging_by_reason: staging.staged_by_reason,
        exhaust_events: staging.events,
    }
}

fn header_lines(s: &Stats) -> Vec<Line<'static>> {
    let daemon_span = match &s.daemon_version {
        Some(v) => Span::styled(format!("daemon v{v} ●"), Style::default().fg(Color::Green)),
        None => Span::styled("daemon down ○", Style::default().fg(Color::Red)),
    };
    let rings = s
        .docs_by_ring
        .iter()
        .map(|(r, n)| format!("r{r}:{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut lines = vec![
        Line::from(vec![
            daemon_span,
            Span::raw(format!(
                "   index: {} blocks [{rings}]   code: {} files / {} symbols   exhaust: {} event(s)",
                s.blocks, s.code_files, s.symbols, s.exhaust_events
            )),
        ]),
        Line::from(format!(
            "ledger: {} session(s), ~{} tokens injected{}",
            s.sessions,
            s.injected_tokens,
            if s.quarantines > 0 {
                format!("   ⚠ {} quarantined corrupt ledger(s)", s.quarantines)
            } else {
                String::new()
            }
        )),
    ];
    // Ring-5 staging: candidates awaiting a distillation session. Yellow the
    // moment anything is pending — quarantine must not be invisible.
    lines.push(if s.staging_total > 0 {
        let reasons = s
            .staging_by_reason
            .iter()
            .map(|(reason, n)| format!("{reason}: {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        Line::from(Span::styled(
            format!("staging: {} candidate(s) [{reasons}]", s.staging_total),
            Style::default().fg(Color::Yellow),
        ))
    } else {
        Line::from(Span::styled(
            "staging: 0 candidates".to_string(),
            Style::default().fg(Color::DarkGray),
        ))
    });
    let failing: Vec<String> = s
        .hooks
        .iter()
        .filter(|(_, fails, _)| *fails >= 3)
        .map(|(n, fails, _)| format!("{n} ({fails}×)"))
        .collect();
    if failing.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("hooks: {} tracked, all healthy", s.hooks.len()),
            Style::default().fg(Color::Green),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("hooks FAILING: {}", failing.join(", ")),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    lines
}

fn ring_color(ring: u8) -> Color {
    match ring {
        0 => Color::Red,
        1 => Color::Magenta,
        2 => Color::Yellow,
        3 => Color::Cyan,
        _ => Color::Gray,
    }
}

struct App {
    query: String,
    hits: Vec<index::Hit>,
    status: String,
}

fn draw(f: &mut Frame, stats: &Stats, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6),
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    f.render_widget(
        Paragraph::new(header_lines(stats)).block(Block::default().borders(Borders::ALL).title(" cfetch ")),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(app.query.as_str())
            .block(Block::default().borders(Borders::ALL).title(" recall (type + Enter) ")),
        chunks[1],
    );
    let items: Vec<ListItem> = app
        .hits
        .iter()
        .map(|h| {
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        h.cite.clone(),
                        Style::default().fg(ring_color(h.ring)).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" {}:{}-{}", h.path, h.start_line, h.end_line)),
                ]),
                Line::from(Span::styled(format!("    {}", h.snippet), Style::default().fg(Color::Gray))),
            ])
        })
        .collect();
    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(format!(" {} ", app.status))),
        chunks[2],
    );
    f.render_widget(
        Paragraph::new(" Enter search · Backspace edit · Esc/Ctrl-C quit ").style(Style::default().fg(Color::DarkGray)),
        chunks[3],
    );
}

pub fn run() -> anyhow::Result<()> {
    let cfg = Config::load()?;
    let native = paths::native_projects_root();
    let conn = index::ensure_fresh(&paths::state_dir(), &cfg.brain_root, Some(&native), &cfg.rings())?;

    // A panicking TUI must never leave the terminal raw.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = crossterm::execute!(out, crossterm::terminal::LeaveAlternateScreen);
        let _ = out.flush();
        default_hook(info);
    }));

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?;

    let mut app = App { query: String::new(), hits: Vec::new(), status: "no query yet".into() };
    let mut stats = gather(&conn);
    let mut last_refresh = std::time::Instant::now();

    let result: anyhow::Result<()> = loop {
        if last_refresh.elapsed() > Duration::from_secs(2) {
            stats = gather(&conn);
            last_refresh = std::time::Instant::now();
        }
        if let Err(e) = terminal.draw(|f| draw(f, &stats, &app)) {
            break Err(e.into());
        }
        if crossterm::event::poll(Duration::from_millis(250))? {
            use crossterm::event::{Event, KeyCode, KeyModifiers};
            if let Event::Key(key) = crossterm::event::read()? {
                if !key.is_press() {
                    continue;
                }
                match key.code {
                    KeyCode::Esc => break Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break Ok(()),
                    KeyCode::Enter => {
                        match index::recall(&conn, &app.query, 20) {
                            Ok(hits) => {
                                app.status = format!("{} hit(s) for \"{}\"", hits.len(), app.query);
                                app.hits = hits;
                            }
                            Err(e) => app.status = format!("recall failed: {e}"),
                        }
                    }
                    KeyCode::Backspace => {
                        app.query.pop();
                    }
                    KeyCode::Char(c) => app.query.push(c),
                    _ => {}
                }
            }
        }
    };

    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), crossterm::terminal::LeaveAlternateScreen)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|sp| sp.content.clone().into_owned())
            .collect()
    }

    #[test]
    fn header_reports_failing_hooks_and_quarantines() {
        let s = Stats {
            daemon_version: None,
            hooks: vec![("stop".into(), 5, None), ("session-start".into(), 0, Some(1))],
            sessions: 2,
            injected_tokens: 1234,
            quarantines: 1,
            docs_by_ring: vec![(1, 2), (3, 400)],
            blocks: 17000,
            code_files: 9000,
            symbols: 240000,
            staging_total: 0,
            staging_by_reason: Vec::new(),
            exhaust_events: 0,
        };
        let text = flat_text(&header_lines(&s));
        assert!(text.contains("daemon down"));
        assert!(text.contains("FAILING: stop (5×)"));
        assert!(text.contains("quarantined"));
        assert!(text.contains("r1:2 r3:400"));
        assert!(text.contains("staging: 0 candidates"), "empty staging still renders");
    }

    #[test]
    fn header_shows_staging_counts_and_exhaust_footprint() {
        let s = Stats {
            daemon_version: Some("0.5.0".into()),
            hooks: Vec::new(),
            sessions: 0,
            injected_tokens: 0,
            quarantines: 0,
            docs_by_ring: Vec::new(),
            blocks: 0,
            code_files: 0,
            symbols: 0,
            staging_total: 6,
            staging_by_reason: vec![
                ("fix-discovered".into(), 3),
                ("recurring-failure".into(), 2),
                ("hot-file".into(), 1),
            ],
            exhaust_events: 4321,
        };
        let lines = header_lines(&s);
        let text = flat_text(&lines);
        assert!(
            text.contains(
                "staging: 6 candidate(s) [fix-discovered: 3, recurring-failure: 2, hot-file: 1]"
            ),
            "got: {text}"
        );
        assert!(text.contains("exhaust: 4321 event(s)"), "got: {text}");
        let staging_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|sp| sp.content.contains("staging:")))
            .unwrap();
        assert_eq!(
            staging_line.spans[0].style.fg,
            Some(Color::Yellow),
            "candidates pending review render yellow"
        );
    }
}
