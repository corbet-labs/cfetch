//! The ranking pipeline: retrieve, optionally rank by vectors, optionally
//! rerank with a cross-encoder.
//!
//! This exists so there is exactly ONE answer to "how does cfetch rank?".
//! Two callers need it and they are on opposite sides of the network: the
//! local CLI on a host that holds its own index, and the serving daemon
//! answering for a host that holds nothing. If each carried its own copy, the
//! same query against the same tree could be ranked two different ways
//! depending on who happened to answer — the coherence failure this codebase
//! exists to prevent, in the ranking rather than in the catalog.
//!
//! Both stages past retrieval are optional and both degrade loudly: the note
//! this returns is the caller's to surface, never to swallow.

use crate::config::Config;
use crate::index::Hit;
use crate::{embed, index, rerank};

/// Ranked hits plus the single note describing every degradation that
/// happened on the way — joined rather than nested, because the caller has
/// exactly one line to show a human and one JSON field to fill.
pub struct Ranked {
    pub hits: Vec<Hit>,
    pub note: Option<String>,
}

/// Ranks `query` to at most `limit` hits using whatever stages this host is
/// configured for.
///
/// Retrieval widens to the reranker's candidate window when one is
/// configured, because a cross-encoder can only promote what retrieval
/// proposed; the answer is cut back to `limit` at the end.
pub fn ranked(
    cfg: &Config,
    conn: &rusqlite::Connection,
    query: &str,
    limit: usize,
    semantic: bool,
    hybrid: bool,
) -> anyhow::Result<Ranked> {
    let mut notes: Vec<String> = Vec::new();

    // A misconfigured reranker can only be fixed by a human, so it is named
    // once and the answer still comes back — the same stance as an
    // unreachable one, which `rerank::apply` reports.
    let reranker = if cfg.rerank.enabled {
        match rerank::RerankClient::new(&cfg.rerank) {
            Ok(c) => Some(c),
            Err(e) => {
                notes.push(format!(
                    "rerank misconfigured ({e}) — answering in retrieval order"
                ));
                None
            }
        }
    } else {
        None
    };
    let retrieve = reranker.as_ref().map_or(limit, |c| c.candidates().max(limit));

    let hits = if semantic || hybrid {
        let out = embed::semantic_hits(cfg, conn, query, retrieve, hybrid)?;
        if let Some(n) = out.note {
            notes.push(n);
        }
        out.hits
    } else {
        index::recall(conn, query, retrieve)?
    };

    let mut hits = match &reranker {
        Some(client) => {
            let out = rerank::apply(client, query, hits, |h: &Hit| h.snippet.clone());
            if let Some(n) = out.note {
                notes.push(n);
            }
            out.hits
        }
        None => hits,
    };
    hits.truncate(limit);

    Ok(Ranked {
        hits,
        note: (!notes.is_empty()).then(|| notes.join("; ")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RerankConfig;
    use crate::testhttp::{http_response, spawn_server};

    /// Five single-line blocks, all matching the query "item", so ranking —
    /// not retrieval — decides what comes back.
    fn five_block_index() -> (tempfile::TempDir, tempfile::TempDir, rusqlite::Connection) {
        let brain = tempfile::tempdir().unwrap();
        let p = brain.path().join("knowledge/a.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "- item one\n- item two\n- item three\n- item four\n- item five\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = index::open(state.path()).unwrap();
        index::scan(&mut conn, brain.path(), None, &crate::config::RingRules::default()).unwrap();
        (brain, state, conn)
    }

    fn cfg_with_rerank(brain: &std::path::Path, url: &str, candidates: usize) -> Config {
        Config {
            brain_root: brain.to_path_buf(),
            rerank: RerankConfig {
                enabled: true,
                endpoint: url.to_string(),
                model: "test-reranker".into(),
                candidates,
                ..RerankConfig::default()
            },
            ..Config::default()
        }
    }

    /// Scores every document by its position, LAST document best, so a
    /// reranked answer is the exact reverse of retrieval order.
    fn reverse_scores(body: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let n = v["documents"].as_array().unwrap().len();
        let rows: Vec<String> = (0..n)
            .map(|i| format!(r#"{{"index":{i},"relevance_score":{}}}"#, i as f32))
            .collect();
        http_response(200, &format!(r#"{{"results":[{}]}}"#, rows.join(",")))
    }

    #[test]
    fn retrieval_widens_to_the_candidate_window_and_the_answer_is_cut_back() {
        let (brain, _state, conn) = five_block_index();
        let (url, bodies, _) = spawn_server(|_, body| reverse_scores(body));
        let cfg = cfg_with_rerank(brain.path(), &url, 5);

        // limit 2, candidates 5: the reranker must SEE all five, or it could
        // never promote the fifth — which is the entire point of the window.
        let out = ranked(&cfg, &conn, "item", 2, false, false).unwrap();
        assert!(out.note.is_none(), "{:?}", out.note);
        let sent: serde_json::Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(sent["documents"].as_array().unwrap().len(), 5, "the window is what gets sent");
        assert_eq!(out.hits.len(), 2, "the caller asked for 2");
        assert!(out.hits[0].snippet.contains("five"), "last by retrieval, first by rerank: {:?}", out.hits[0]);
    }

    #[test]
    fn without_a_reranker_retrieval_never_widens() {
        let (brain, _state, conn) = five_block_index();
        let cfg = Config { brain_root: brain.path().to_path_buf(), ..Config::default() };
        let out = ranked(&cfg, &conn, "item", 2, false, false).unwrap();
        assert_eq!(out.hits.len(), 2);
        assert!(out.note.is_none());
        assert!(out.hits[0].snippet.contains("one"), "plain retrieval order: {:?}", out.hits[0]);
    }

    #[test]
    fn an_unreachable_reranker_still_answers_and_says_so() {
        let (brain, _state, conn) = five_block_index();
        let cfg = cfg_with_rerank(brain.path(), "http://127.0.0.1:1/v1", 5);
        let out = ranked(&cfg, &conn, "item", 3, false, false).unwrap();
        assert_eq!(out.hits.len(), 3, "the answer survives the second stage failing");
        let note = out.note.expect("degradation is never silent");
        assert!(note.contains("rerank unavailable"), "{note}");
    }

    #[test]
    fn a_misconfigured_reranker_is_named_once_and_the_answer_comes_back() {
        let (brain, _state, conn) = five_block_index();
        // Enabled but with no model: only a human can fix this, so it is
        // reported rather than retried per query.
        let mut cfg = cfg_with_rerank(brain.path(), "http://127.0.0.1:1/v1", 5);
        cfg.rerank.model = String::new();
        let out = ranked(&cfg, &conn, "item", 3, false, false).unwrap();
        assert_eq!(out.hits.len(), 3);
        let note = out.note.expect("a misconfiguration must reach the operator");
        assert!(note.contains("rerank misconfigured"), "{note}");
        assert!(!note.contains('\n'), "one line: {note}");
    }
}
