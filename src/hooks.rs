//! Hook entrypoints. Contract: read stdin, act within the latency budget, emit
//! at most one JSON object, ALWAYS exit 0 — a hook failure must degrade to
//! silence, never to a broken session.

use std::time::Duration;

use crate::config::Config;
use crate::hook_io::{Emit, HookEvent};
use crate::{daemon, heartbeat, ledger, resident};

const DAEMON_BUDGET: Duration = Duration::from_millis(250);

/// Dispatches a hook event by name. Never returns an error to the harness.
pub fn run(event_name: &str) {
    let event = HookEvent::from_stdin();
    let result = match event_name {
        "session-start" => session_start(&event),
        // Milestone-1 skeletons: prove liveness, do nothing else yet.
        "pre-tool" | "post-tool" | "stop" | "precompact" => Ok(()),
        other => Err(anyhow::anyhow!("unknown hook: {other}")),
    };
    match result {
        Ok(()) => heartbeat::record_ok(event_name),
        Err(e) => heartbeat::record_error(event_name, &e.to_string()),
    }
    // Exit 0 unconditionally — see module doc.
}

/// Direct-read fallback with a hard deadline: the tree may be NFS, and a hung
/// mount must not eat the whole hook timeout. The worker thread is detached on
/// overrun.
fn resident_with_deadline(cfg: &Config) -> String {
    let cfg = cfg.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(resident::build(&cfg).text);
    });
    rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
}

fn session_start(event: &HookEvent) -> anyhow::Result<()> {
    // Subagents inherit fresh context on purpose; the resident set is for the
    // primary session (a fork re-injecting rings would double-pay the budget).
    if event.is_subagent() {
        return Ok(());
    }

    let mut emit = Emit::new("SessionStart");

    // Everything below the digest is CONFIG-INDEPENDENT and must reach the
    // model even when the config is the thing that broke — otherwise the one
    // surface built to announce breakage is suppressed by the breakage.
    let cfg = Config::load();
    let (digest, max_sessions) = match &cfg {
        Ok(cfg) => {
            // Prefer the warm daemon; fall back to a bounded direct read —
            // session start works with no daemon at all.
            let digest = match daemon::call("resident", DAEMON_BUDGET) {
                Some(r) if r.ok => r.digest.unwrap_or_default(),
                _ => resident_with_deadline(cfg),
            };
            (digest, cfg.ledger_max_sessions)
        }
        Err(_) => (String::new(), 200),
    };

    let reason = event.start_reason();
    if !digest.is_empty() {
        if reason == "compact" || reason == "resume" {
            emit.add_context(format!(
                "[cfetch resident memory (rings 0-1), re-injected after {reason}]\n{digest}"
            ));
        } else {
            emit.add_context(format!("[cfetch resident memory (rings 0-1)]\n{digest}"));
        }
    }

    if let Err(e) = &cfg {
        emit.add_context(format!(
            "[cfetch degraded: config unusable ({e}) — memory injection disabled; run `cfetch selfcheck`]"
        ));
    }
    let degraded = heartbeat::degraded();
    if !degraded.is_empty() {
        let names: Vec<String> = degraded.iter().map(|(n, _)| n.clone()).collect();
        emit.add_context(format!(
            "[cfetch degraded: hook(s) {} failing repeatedly — memory capture may be incomplete; run `cfetch status`]",
            names.join(", ")
        ));
    }

    let emitted = emit.finish();
    ledger::book(event.session(), "resident-digest", emitted, max_sessions);
    // The config failure still counts as a hook failure for the heartbeat.
    cfg.map(|_| ())
}
