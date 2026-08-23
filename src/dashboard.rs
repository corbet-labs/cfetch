//! Ratatui terminal dashboard: read-only brain health, live recall, and the
//! supervised maintenance inbox. One TUI, no daemon required.
//!
//! The numbers come from whatever holds this host's index: the LOCAL index on
//! a storage host, the SERVING host on a none-tier one. It opens a local index
//! only where one legitimately exists — a none-tier host that opened one would
//! render a second, silently stale truth beside its serving host's, which is
//! the drift this screen exists to expose. When neither can answer, the header
//! says so; zeros are never printed in place of an unavailable index.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};

use crate::config::{ClientServingConfig, Config};
use crate::maintenance_inbox::{Inbox, Tone};
use crate::{daemon, exhaust, heartbeat, index, ledger, maintenance, paths, runtime_status, serve};

/// Remote budget for the dashboard's own calls. Deliberately shorter than the
/// CLI's: this screen refreshes on a timer and must not freeze on a slow drain
/// barrier.
const REMOTE_STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// Whoever can answer this host's queries.
enum Source {
    /// This host holds the index.
    Local {
        conn: rusqlite::Connection,
        /// Serving identity, when this host also serves.
        origin: Option<String>,
    },
    /// none-tier: the serving host answers, and no local index is opened.
    Served(ClientServingConfig),
    /// Neither — the reason is shown instead of numbers.
    Unusable(String),
}

/// Picks the source from config alone. Infallible on purpose: a broken index
/// is something to REPORT on the screen, not a reason to refuse to draw it.
fn open_source(cfg: &Config, state: &Path) -> Source {
    if let Some(cs) = &cfg.client.serving {
        return Source::Served(cs.clone());
    }
    let native = paths::native_projects_root();
    match index::ensure_fresh(state, &cfg.brain_root, Some(&native), &cfg.rings()) {
        Ok(conn) => Source::Local {
            conn,
            origin: cfg.serve.enabled.then(|| serve::origin_of(cfg)),
        },
        Err(e) => Source::Unusable(format!("local index unavailable: {e}")),
    }
}

/// Every remote failure names the serving host: a none-tier host has no local
/// data to fall back on and must never merely look empty.
fn served(
    cs: &ClientServingConfig,
    body: serde_json::Value,
    timeout: Duration,
) -> anyhow::Result<daemon::Response> {
    serve::client_call(cs, body, timeout)
        .map_err(|e| anyhow::anyhow!("serving host {} unavailable: {e}", cs.addr))
}

/// What the header can honestly say about the index behind this host.
#[derive(Debug)]
enum IndexView {
    Local {
        docs_by_ring: Vec<(u8, i64)>,
        blocks: i64,
        code_files: i64,
        symbols: i64,
        /// Serving identity, when this host also serves.
        origin: Option<String>,
        generation: u64,
    },
    Served {
        origin: String,
        generation: u64,
        fresh: bool,
    },
    Unavailable {
        reason: String,
    },
}

struct Stats {
    runtime: runtime_status::RuntimeStatusV1,
    daemon_version: Option<String>,
    /// Expected-versus-observed, never a bare failure count: a hook that has
    /// never fired must not share a rendering with one that runs cleanly.
    hooks: heartbeat::Liveness,
    sessions: usize,
    injected_tokens: u64,
    /// Ledger streams this binary refused to read (a future format version).
    unreadable_streams: usize,
    index: IndexView,
    staging_total: i64,
    staging_by_reason: Vec<(String, i64)>,
    maintenance_pending: usize,
    maintenance_applied: usize,
    exhaust_bytes: u64,
}

fn index_view(source: &Source) -> IndexView {
    match source {
        Source::Local { conn, origin } => {
            let docs_by_ring = conn
                .prepare("SELECT ring, count(*) FROM docs GROUP BY ring ORDER BY ring")
                .and_then(|mut s| {
                    s.query_map([], |r| Ok((r.get::<_, i64>(0)? as u8, r.get::<_, i64>(1)?)))
                        .map(|rows| rows.filter_map(Result::ok).collect())
                })
                .unwrap_or_default();
            let one = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
            IndexView::Local {
                docs_by_ring,
                blocks: one("SELECT count(*) FROM blocks"),
                code_files: one("SELECT count(*) FROM code_files"),
                symbols: one("SELECT count(*) FROM symbols"),
                origin: origin.clone(),
                generation: index::generation(conn),
            }
        }
        // One tiny barrier-gated round trip per refresh: the generation op is
        // the same coherence label the CLI footer prints.
        Source::Served(cs) => {
            match served(cs, serde_json::json!({"op": "generation"}), REMOTE_STATUS_TIMEOUT) {
                Ok(r) => IndexView::Served {
                    origin: r.origin.unwrap_or_else(|| cs.addr.clone()),
                    generation: r.generation.unwrap_or(0),
                    fresh: r.fresh.unwrap_or(false),
                },
                Err(e) => IndexView::Unavailable { reason: e.to_string() },
            }
        }
        Source::Unusable(reason) => IndexView::Unavailable { reason: reason.clone() },
    }
}

fn gather(cfg: &Config, source: &Source, state: &Path) -> Stats {
    // The ledger and ring-5/6 figures come from the TREE, so this pane shows
    // the whole fleet's numbers, not just this machine's.
    let loaded = ledger::read(&paths::logs_dir(&cfg.brain_root));
    let l = loaded.ledger;
    // Read-only, fail-silent: an absent tree simply reports zeros.
    let staging = exhaust::Exhaust::from_config(cfg).stats();
    let daemon_probe = daemon::call("ping", Duration::from_millis(200));
    let daemon_running = daemon_probe.is_some();
    let daemon_version = daemon_probe.and_then(|response| response.version);
    let mut runtime =
        runtime_status::refresh_static().unwrap_or_else(|_| runtime_status::load_cached());
    runtime_status::apply_daemon_observation(&mut runtime, daemon_running);
    Stats {
        runtime,
        daemon_version,
        hooks: heartbeat::liveness_in(state),
        sessions: l.sessions.len(),
        injected_tokens: l
            .sessions
            .values()
            .flat_map(|s| s.by_source.values())
            .map(|t| t.tokens_estimated)
            .sum(),
        unreadable_streams: loaded.unreadable.len(),
        index: index_view(source),
        staging_total: staging.staged_total,
        staging_by_reason: staging.staged_by_reason,
        maintenance_pending: maintenance::pending_count(cfg),
        maintenance_applied: maintenance::applied_count(cfg),
        exhaust_bytes: staging.bytes,
    }
}

/// The index half of the header's first line, with the colour that says how
/// much to trust it.
fn index_span(v: &IndexView) -> Span<'static> {
    match v {
        // Generation 0 means no scan has ever committed here, so every count
        // beside it would be an unmeasured zero. An empty brain and an index
        // nobody has built must not print the same "0 blocks".
        IndexView::Local { generation: 0, .. } => Span::styled(
            format!("   {}", heartbeat::IndexLiveness::NeverScanned.describe()),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        IndexView::Local { docs_by_ring, blocks, code_files, symbols, origin, generation } => {
            let rings = docs_by_ring
                .iter()
                .map(|(r, n)| format!("r{r}:{n}"))
                .collect::<Vec<_>>()
                .join(" ");
            let serving = match origin {
                Some(o) => format!("   serving as {o} (generation {generation})"),
                None => format!("   generation {generation}"),
            };
            Span::raw(format!(
                "   index: {blocks} blocks [{rings}]   code: {code_files} files / {symbols} symbols{serving}"
            ))
        }
        IndexView::Served { origin, generation, fresh } => Span::styled(
            if *fresh {
                format!("   served by {origin} (generation {generation}, fresh)")
            } else {
                format!("   served by {origin} (generation {generation}) — STALE")
            },
            Style::default().fg(if *fresh { Color::Green } else { Color::Yellow }),
        ),
        IndexView::Unavailable { reason } => Span::styled(
            format!("   NO INDEX: {reason}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    }
}

fn header_lines(s: &Stats) -> Vec<Line<'static>> {
    let daemon_span = match &s.daemon_version {
        Some(v) => Span::styled(format!("daemon v{v} ●"), Style::default().fg(Color::Green)),
        None => Span::styled("daemon down ○", Style::default().fg(Color::Red)),
    };
    let runtime_style = match s.runtime.service.state {
        runtime_status::ServiceState::Ready => Style::default().fg(Color::Green),
        runtime_status::ServiceState::Degraded => Style::default().fg(Color::Yellow),
        runtime_status::ServiceState::Unavailable => {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        }
    };
    let mut lines = vec![
        Line::from(Span::styled(
            runtime_status::render_line_with_width(&s.runtime, Some(180)),
            runtime_style,
        )),
        Line::from(vec![
            daemon_span,
            index_span(&s.index),
            Span::raw(format!("   exhaust: {}", crate::jsonl::human_bytes(s.exhaust_bytes))),
        ]),
        Line::from(format!(
            "ledger: {} session(s), ~{} tokens injected{}",
            s.sessions,
            s.injected_tokens,
            if s.unreadable_streams > 0 {
                format!("   ⚠ {} unreadable ledger stream(s)", s.unreadable_streams)
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
            format!(
                "staging: {} candidate(s) [{reasons}]   maintenance: {} pending / {} applied",
                s.staging_total, s.maintenance_pending, s.maintenance_applied
            ),
            Style::default().fg(Color::Yellow),
        ))
    } else if s.exhaust_bytes == 0 {
        // Staging is fed by the ring-6 capture hooks. With not one byte of
        // exhaust anywhere in the tree, nothing has ever run the flagging
        // traps, so this zero is the absence of a measurement.
        Line::from(Span::styled(
            format!(
                "staging: 0 candidates — UNOBSERVED: no ring-6 exhaust written   maintenance: {} pending / {} applied",
                s.maintenance_pending, s.maintenance_applied
            ),
            Style::default().fg(Color::Yellow),
        ))
    } else if s.maintenance_pending > 0 || s.maintenance_applied > 0 {
        Line::from(Span::styled(
            format!(
                "staging: 0 candidates (measured)   maintenance: {} pending / {} applied",
                s.maintenance_pending, s.maintenance_applied
            ),
            Style::default().fg(Color::Yellow),
        ))
    } else {
        Line::from(Span::styled(
            "staging: 0 candidates (measured)   maintenance: 0 pending / 0 applied".to_string(),
            Style::default().fg(Color::DarkGray),
        ))
    });
    lines.push(Line::from(Span::styled(
        s.hooks.summary(),
        match s.hooks.severity() {
            heartbeat::Severity::Healthy => Style::default().fg(Color::Green),
            heartbeat::Severity::Unobserved => Style::default().fg(Color::Yellow),
            heartbeat::Severity::Failing => {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            }
        },
    )));
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

/// The recall pane's query, routed exactly like `cfetch recall`: local index
/// on a storage host, serving host on a none-tier one.
fn recall_hits(source: &Source, query: &str, limit: usize) -> anyhow::Result<Vec<serve::WireHit>> {
    match source {
        Source::Local { conn, .. } => {
            Ok(index::recall(conn, query, limit)?.into_iter().map(Into::into).collect())
        }
        Source::Served(cs) => {
            let body = serde_json::json!({"op": "recall", "query": query, "limit": limit});
            Ok(served(cs, body, serve::QUERY_TIMEOUT)?.hits.unwrap_or_default())
        }
        Source::Unusable(reason) => anyhow::bail!("{reason}"),
    }
}

struct App {
    pane: Pane,
    query: String,
    hits: Vec<serve::WireHit>,
    status: String,
    inbox: Inbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pane {
    Recall,
    Maintenance,
}

impl Pane {
    fn toggle(self) -> Self {
        match self {
            Self::Recall => Self::Maintenance,
            Self::Maintenance => Self::Recall,
        }
    }
}

fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Normal => Style::default(),
        Tone::Muted => Style::default().fg(Color::DarkGray),
        Tone::Good => Style::default().fg(Color::Green),
        Tone::Warning => Style::default().fg(Color::Yellow),
        Tone::Error => Style::default().fg(Color::Red),
        Tone::Accent => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    }
}

fn draw_recall(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);
    f.render_widget(
        Paragraph::new(app.query.as_str())
            .block(Block::default().borders(Borders::ALL).title(" recall (type + Enter) ")),
        chunks[0],
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
        chunks[1],
    );
}

fn draw_maintenance(f: &mut Frame, area: Rect, inbox: &Inbox) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
        .split(area);
    let rows: Vec<ListItem> = inbox
        .rows()
        .into_iter()
        .map(|row| {
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!(" {:<9} ", row.badge), tone_style(row.tone)),
                    Span::styled(row.summary, Style::default().fg(Color::Gray)),
                ]),
                Line::from(Span::styled(format!("   {}", row.id), Style::default().fg(Color::DarkGray))),
            ])
        })
        .collect();
    let mut state = ListState::default();
    state.select(inbox.selected());
    f.render_stateful_widget(
        List::new(rows)
            .block(Block::default().borders(Borders::ALL).title(format!(" {} ", inbox.status)))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("›"),
        chunks[0],
        &mut state,
    );

    let detail = inbox
        .detail
        .lines
        .iter()
        .map(|line| Line::from(Span::styled(line.text.clone(), tone_style(line.tone))))
        .collect::<Vec<_>>();
    f.render_widget(
        Paragraph::new(detail)
            .block(Block::default().borders(Borders::ALL).title(inbox.detail.title.clone()))
            .wrap(Wrap { trim: false })
            .scroll((inbox.detail_scroll, 0)),
        chunks[1],
    );
}

fn draw(f: &mut Frame, stats: &Stats, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
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
        Tabs::new(["Recall", "Maintenance inbox"])
            .select(match app.pane {
                Pane::Recall => 0,
                Pane::Maintenance => 1,
            })
            .block(Block::default().borders(Borders::ALL).title(" Tab switches view "))
            .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        chunks[1],
    );
    match app.pane {
        Pane::Recall => draw_recall(f, chunks[2], app),
        Pane::Maintenance => draw_maintenance(f, chunks[2], &app.inbox),
    }
    let help = match app.pane {
        Pane::Recall => " Tab inbox · Enter search · Backspace edit · Esc/Ctrl-C quit ",
        Pane::Maintenance => {
            " Tab recall · ↑/↓ or j/k select · PgUp/PgDn scroll · r refresh · Esc/Ctrl-C quit "
        }
    };
    f.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        chunks[3],
    );
}

pub fn run() -> anyhow::Result<()> {
    let cfg = Config::load()?;
    let state = paths::state_dir();
    let source = open_source(&cfg, &state);

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

    let can_verify_locally = cfg.client.serving.is_none();
    let mut app = App {
        pane: Pane::Recall,
        query: String::new(),
        hits: Vec::new(),
        status: "no query yet".into(),
        inbox: Inbox::load(&cfg, can_verify_locally),
    };
    let mut stats = gather(&cfg, &source, &state);
    let mut last_refresh = std::time::Instant::now();

    let result: anyhow::Result<()> = loop {
        if last_refresh.elapsed() > Duration::from_secs(2) {
            stats = gather(&cfg, &source, &state);
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
                if key.code == KeyCode::Tab {
                    app.pane = app.pane.toggle();
                    continue;
                }
                match key.code {
                    KeyCode::Esc => break Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break Ok(()),
                    KeyCode::Enter if app.pane == Pane::Recall => {
                        match recall_hits(&source, &app.query, 20) {
                            Ok(hits) => {
                                app.status = format!("{} hit(s) for \"{}\"", hits.len(), app.query);
                                app.hits = hits;
                            }
                            Err(e) => app.status = format!("recall failed: {e}"),
                        }
                    }
                    KeyCode::Backspace if app.pane == Pane::Recall => {
                        app.query.pop();
                    }
                    KeyCode::Char(c) if app.pane == Pane::Recall => app.query.push(c),
                    KeyCode::Down | KeyCode::Char('j') if app.pane == Pane::Maintenance => {
                        app.inbox.select_next(&cfg, can_verify_locally);
                    }
                    KeyCode::Up | KeyCode::Char('k') if app.pane == Pane::Maintenance => {
                        app.inbox.select_previous(&cfg, can_verify_locally);
                    }
                    KeyCode::Home if app.pane == Pane::Maintenance => {
                        app.inbox.select_first(&cfg, can_verify_locally);
                    }
                    KeyCode::End if app.pane == Pane::Maintenance => {
                        app.inbox.select_last(&cfg, can_verify_locally);
                    }
                    KeyCode::PageDown if app.pane == Pane::Maintenance => {
                        app.inbox.scroll_down(12);
                    }
                    KeyCode::PageUp if app.pane == Pane::Maintenance => {
                        app.inbox.scroll_up(12);
                    }
                    KeyCode::Char('r') if app.pane == Pane::Maintenance => {
                        app.inbox.refresh(&cfg, can_verify_locally);
                        stats = gather(&cfg, &source, &state);
                        last_refresh = std::time::Instant::now();
                    }
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

    fn local_index() -> IndexView {
        IndexView::Local {
            docs_by_ring: vec![(1, 2), (3, 400)],
            blocks: 17000,
            code_files: 9000,
            symbols: 240000,
            origin: None,
            generation: 5,
        }
    }

    fn liveness(states: &[(&str, heartbeat::HookState)]) -> heartbeat::Liveness {
        heartbeat::Liveness {
            hooks: heartbeat::REGISTERED_HOOKS
                .iter()
                .map(|name| heartbeat::HookLiveness {
                    name: (*name).to_string(),
                    registered: true,
                    state: states
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, s)| s.clone())
                        .unwrap_or(heartbeat::HookState::Unobserved),
                })
                .collect(),
        }
    }

    fn all_reporting() -> heartbeat::Liveness {
        let states: Vec<(&str, heartbeat::HookState)> = heartbeat::REGISTERED_HOOKS
            .iter()
            .map(|n| (*n, heartbeat::HookState::Healthy { last_ok: 1 }))
            .collect();
        liveness(&states)
    }

    #[test]
    fn none_tier_dashboard_never_opens_a_local_index() {
        // The defect: the dashboard opened a LOCAL index unconditionally, so a
        // none-tier host rendered an empty second truth next to its serving
        // host's real one.
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config { brain_root: dir.path().join("brain"), ..Config::default() };
        cfg.client.serving = Some(ClientServingConfig {
            addr: "198.51.100.7:9737".to_string(),
            token_file: dir.path().join("absent-token"),
        });
        let source = open_source(&cfg, dir.path());
        let stats = gather(&cfg, &source, dir.path());
        assert!(
            !dir.path().join("index.db").exists(),
            "a none-tier host must open (and create) NO local index"
        );
        match &stats.index {
            IndexView::Unavailable { reason } => {
                assert!(reason.contains("198.51.100.7:9737"), "name the serving host: {reason}");
            }
            other => panic!("an unreachable serving host must be reported, got {other:?}"),
        }
        let text = flat_text(&header_lines(&stats));
        assert!(text.contains("198.51.100.7:9737"), "{text}");
        assert!(!text.contains("0 blocks"), "zeros must never stand in for a remote index: {text}");
    }

    #[test]
    fn recall_failures_name_what_could_not_answer() {
        // The recall pane routes like the CLI, so its failures must be as
        // explicit: a none-tier host has nothing local to fall back on.
        let dir = tempfile::tempdir().unwrap();
        let cs = ClientServingConfig {
            addr: "198.51.100.7:9737".to_string(),
            token_file: dir.path().join("absent-token"),
        };
        let err = recall_hits(&Source::Served(cs), "anything", 5).unwrap_err().to_string();
        assert!(err.contains("198.51.100.7:9737"), "name the serving host: {err}");
        let err = recall_hits(&Source::Unusable("local index unavailable: boom".into()), "x", 5)
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom"), "{err}");
    }

    #[test]
    fn header_shows_the_serving_origin_and_generation() {
        let s = Stats {
            runtime: runtime_status::RuntimeStatusV1::default(),
            daemon_version: Some("0.5.0".into()),
            hooks: all_reporting(),
            sessions: 0,
            injected_tokens: 0,
            unreadable_streams: 0,
            index: IndexView::Served {
                origin: "storage-1".into(),
                generation: 91,
                fresh: true,
            },
            staging_total: 0,
            staging_by_reason: Vec::new(),
            maintenance_pending: 0,
            maintenance_applied: 0,
            exhaust_bytes: 0,
        };
        let text = flat_text(&header_lines(&s));
        assert!(text.contains("cfetch ● memory:local"), "{text}");
        assert!(text.contains("served by storage-1"), "{text}");
        assert!(text.contains("generation 91"), "{text}");
        // A stale remote answer must say so rather than look authoritative.
        let stale = Stats {
            index: IndexView::Served { origin: "storage-1".into(), generation: 91, fresh: false },
            ..s
        };
        assert!(flat_text(&header_lines(&stale)).contains("STALE"), "staleness must be labeled");
    }

    #[test]
    fn header_reports_failing_hooks_and_unreadable_streams() {
        let s = Stats {
            runtime: runtime_status::RuntimeStatusV1::default(),
            daemon_version: None,
            hooks: liveness(&[
                (
                    "stop",
                    heartbeat::HookState::Failing {
                        consecutive: 5,
                        last_error: Some("boom".into()),
                        last_ok: None,
                    },
                ),
                ("session-start", heartbeat::HookState::Healthy { last_ok: 1 }),
            ]),
            sessions: 2,
            injected_tokens: 1234,
            unreadable_streams: 1,
            index: local_index(),
            staging_total: 0,
            staging_by_reason: Vec::new(),
            maintenance_pending: 0,
            maintenance_applied: 0,
            exhaust_bytes: 0,
        };
        let text = flat_text(&header_lines(&s));
        assert!(text.contains("daemon down"));
        assert!(text.contains("FAILING: stop (5×)"));
        assert!(text.contains("unreadable ledger stream(s)"));
        assert!(text.contains("r1:2 r3:400"));
        assert!(text.contains("staging: 0 candidates"), "empty staging still renders");
        // Four hooks in that heartbeat have no record at all, and the failing
        // one must not hide them.
        assert!(text.contains("NEVER OBSERVED: user-prompt"), "{text}");
    }

    #[test]
    fn silent_hooks_never_render_as_all_healthy() {
        // The defect: the header counted the heartbeat's own entries and
        // called the lot healthy, so a hook that had never fired since
        // install was indistinguishable from one that had just succeeded.
        let base = Stats {
            runtime: runtime_status::RuntimeStatusV1::default(),
            daemon_version: Some("0.5.0".into()),
            hooks: liveness(&[("session-start", heartbeat::HookState::Healthy { last_ok: 1 })]),
            sessions: 0,
            injected_tokens: 0,
            unreadable_streams: 0,
            index: local_index(),
            staging_total: 0,
            staging_by_reason: Vec::new(),
            maintenance_pending: 0,
            maintenance_applied: 0,
            exhaust_bytes: 12,
        };
        let lines = header_lines(&base);
        let text = flat_text(&lines);
        assert!(!text.contains("all healthy"), "one reporting hook is not a healthy set: {text}");
        assert!(text.contains("1 of 6 registered hook(s) reporting"), "{text}");
        assert!(text.contains("NEVER OBSERVED: user-prompt, pre-tool, post-tool, stop"), "{text}");
        let hook_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|sp| sp.content.contains("NEVER OBSERVED")))
            .unwrap();
        assert_eq!(
            hook_line.spans[0].style.fg,
            Some(Color::Yellow),
            "unobserved is neither green nor red"
        );

        // Only a complete set earns the green line.
        let healthy = Stats { hooks: all_reporting(), ..base };
        let lines = header_lines(&healthy);
        let text = flat_text(&lines);
        assert!(text.contains("all 6 registered hook(s) reporting, healthy"), "{text}");
        let hook_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|sp| sp.content.contains("hooks:")))
            .unwrap();
        assert_eq!(hook_line.spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn an_index_nobody_has_scanned_is_not_an_empty_one() {
        // Generation 0 is "never scanned". Rendering its counts would print
        // "0 blocks", which reads as a brain with nothing in it.
        let never = Stats {
            runtime: runtime_status::RuntimeStatusV1::default(),
            daemon_version: Some("0.5.0".into()),
            hooks: all_reporting(),
            sessions: 0,
            injected_tokens: 0,
            unreadable_streams: 0,
            index: IndexView::Local {
                docs_by_ring: Vec::new(),
                blocks: 0,
                code_files: 0,
                symbols: 0,
                origin: None,
                generation: 0,
            },
            staging_total: 0,
            staging_by_reason: Vec::new(),
            maintenance_pending: 0,
            maintenance_applied: 0,
            exhaust_bytes: 4096,
        };
        let text = flat_text(&header_lines(&never));
        assert!(text.contains("NEVER SCANNED"), "{text}");
        assert!(!text.contains("0 blocks"), "an unbuilt catalog must not print counts: {text}");

        // A scanned but genuinely empty tree keeps its zeros: that one IS a
        // measurement.
        let empty = Stats {
            index: IndexView::Local {
                docs_by_ring: Vec::new(),
                blocks: 0,
                code_files: 0,
                symbols: 0,
                origin: None,
                generation: 2,
            },
            ..never
        };
        let text = flat_text(&header_lines(&empty));
        assert!(text.contains("0 blocks"), "{text}");
        assert!(!text.contains("NEVER SCANNED"), "{text}");
    }

    #[test]
    fn zero_staging_with_no_exhaust_on_disk_is_unobserved() {
        let base = Stats {
            runtime: runtime_status::RuntimeStatusV1::default(),
            daemon_version: Some("0.5.0".into()),
            hooks: all_reporting(),
            sessions: 0,
            injected_tokens: 0,
            unreadable_streams: 0,
            index: local_index(),
            staging_total: 0,
            staging_by_reason: Vec::new(),
            maintenance_pending: 0,
            maintenance_applied: 0,
            exhaust_bytes: 0,
        };
        let text = flat_text(&header_lines(&base));
        assert!(text.contains("UNOBSERVED"), "no capture stream means no examined turns: {text}");

        let captured = Stats { exhaust_bytes: 8192, ..base };
        let text = flat_text(&header_lines(&captured));
        assert!(text.contains("staging: 0 candidates (measured)"), "{text}");
        assert!(!text.contains("UNOBSERVED"), "{text}");
    }

    #[test]
    fn header_shows_staging_counts_and_exhaust_footprint() {
        let s = Stats {
            runtime: runtime_status::RuntimeStatusV1::default(),
            daemon_version: Some("0.5.0".into()),
            hooks: all_reporting(),
            sessions: 0,
            injected_tokens: 0,
            unreadable_streams: 0,
            index: IndexView::Local {
                docs_by_ring: Vec::new(),
                blocks: 0,
                code_files: 0,
                symbols: 0,
                origin: None,
                generation: 2,
            },
            staging_total: 6,
            staging_by_reason: vec![
                ("fix-discovered".into(), 3),
                ("recurring-failure".into(), 2),
                ("hot-file".into(), 1),
            ],
            maintenance_pending: 2,
            maintenance_applied: 1,
            exhaust_bytes: 4321 * 1024,
        };
        let lines = header_lines(&s);
        let text = flat_text(&lines);
        assert!(
            text.contains(
                "staging: 6 candidate(s) [fix-discovered: 3, recurring-failure: 2, hot-file: 1]"
            ),
            "got: {text}"
        );
        assert!(text.contains("exhaust: 4.2 MiB"), "got: {text}");
        assert!(text.contains("maintenance: 2 pending / 1 applied"), "got: {text}");
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
