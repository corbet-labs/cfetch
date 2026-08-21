//! Embeddings client for semantic recall — an OpenAI-compatible
//! `POST {endpoint}/embeddings` with `{"model", "input": [texts]}`.
//!
//! NEVER called from hook entrypoints. Hooks sit on the interactive path and
//! must not spend network time; embedding happens only from the CLI
//! (`cfetch embed-index`, `cfetch recall --semantic/--hybrid`) or the daemon.
//!
//! The endpoint URL comes from the config file — a file agents write — so it
//! is SSRF-guarded at use: https or loopback only, private/link-local/
//! metadata ranges refused, redirects disabled (a 3xx must never be able to
//! walk a request, and its Authorization header, somewhere else).
//!
//! Auth: `embeddings.api_key_env` names an environment VARIABLE holding the
//! key (never the key itself in config) -> `Authorization: Bearer` header.
//! Timeouts: `embeddings.timeout_secs` (default 10 s) bounds one interactive
//! request — a backend that slow is down, not busy. The embed-index batch
//! path scales that bound per batched item (`batch_timeout`): a 64-block
//! batch on a CPU backend is busy, not down, and timing it out only to
//! resend the identical batch is a livelock.

use anyhow::Context as _;
use rusqlite::Connection;

use crate::config::{Config, EmbeddingsConfig};
use crate::index;

/// (scheme, host) of a URL, lowercased, without pulling in a URL crate.
/// Userinfo is refused outright: `https://safe.example@evil.host/` parses two
/// different ways in two different libraries — the classic SSRF confusion.
fn split_url(url: &str) -> anyhow::Result<(String, String)> {
    let (scheme, rest) = url
        .split_once("://")
        .with_context(|| format!("endpoint {url:?} is not a scheme://host URL"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    anyhow::ensure!(!authority.contains('@'), "endpoint URL must not contain userinfo");
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        // [ipv6]:port
        bracketed
            .split_once(']')
            .context("endpoint URL has an unclosed IPv6 bracket")?
            .0
            .to_string()
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h).to_string()
    };
    anyhow::ensure!(!host.is_empty(), "endpoint URL has no host");
    Ok((scheme.to_ascii_lowercase(), host.to_ascii_lowercase()))
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host.parse::<std::net::IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

/// Address ranges no config value may point at (loopback is checked first
/// and exempt). ALL non-public ranges are refused by default, including
/// 100.64/10 (CGNAT) — in an arbitrary deployment that space can be someone
/// else's internal network. Operators whose infrastructure legitimately
/// lives in such a range (e.g. a WireGuard/mesh overlay) opt in explicitly
/// with `embeddings.allow_hosts` in the config.
fn forbidden_range(host: &str) -> Option<&'static str> {
    let ip: std::net::IpAddr = host.parse().ok()?;
    match ip {
        std::net::IpAddr::V4(v4) => {
            if v4.is_private() {
                Some("private range (RFC 1918)")
            } else if (v4.octets()[0] == 100) && (v4.octets()[1] & 0xc0) == 64 {
                Some("shared/CGNAT range (100.64.0.0/10)")
            } else if v4.is_link_local() {
                Some("link-local/metadata range (169.254.0.0/16)")
            } else if v4.is_unspecified() || v4.is_broadcast() {
                Some("non-routable address")
            } else {
                None
            }
        }
        std::net::IpAddr::V6(v6) => {
            if v6.is_unspecified() {
                Some("non-routable address")
            } else if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                Some("IPv6 link-local")
            } else if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                Some("IPv6 unique-local")
            } else {
                None
            }
        }
    }
}

/// Validates a config-supplied endpoint URL against the egress policy:
/// http/https only, loopback always allowed, everything else must be https
/// AND outside private (RFC 1918), shared/CGNAT (100.64/10), link-local/
/// metadata, and IPv6 unique-local/link-local ranges. `allow_hosts` is the
/// operator's EXPLICIT per-host exemption from the range refusal (never from
/// the https requirement): mesh overlays and lab networks opt in by listing
/// the exact host, general deployments stay closed by default.
pub fn check_endpoint(url: &str, allow_hosts: &[String]) -> anyhow::Result<()> {
    let (scheme, host) = split_url(url)?;
    anyhow::ensure!(
        scheme == "http" || scheme == "https",
        "endpoint scheme {scheme:?} refused (http/https only)"
    );
    if is_loopback_host(&host) {
        return Ok(());
    }
    let exempted = allow_hosts.iter().any(|a| a.eq_ignore_ascii_case(&host));
    if !exempted && let Some(reason) = forbidden_range(&host) {
        anyhow::bail!(
            "endpoint host {host} refused: {reason} (add it to embeddings.allow_hosts to permit deliberately)"
        );
    }
    anyhow::ensure!(scheme == "https", "non-loopback endpoint must be https (got http://{host})");
    Ok(())
}

/// Extra timeout allowance per batched input on the embed-index path.
/// Interactive recall (one input) keeps the tight base bound.
const PER_ITEM: std::time::Duration = std::time::Duration::from_secs(2);

/// The embed-index batch bound: base + per-item.
fn batch_timeout(base: std::time::Duration, items: usize) -> std::time::Duration {
    base + PER_ITEM * items as u32
}

/// Resolves `api_key_env` (an environment variable NAME) to a ready
/// `Bearer …` header value. Empty config = no auth. A value that cannot be
/// an env var name is refused loudly — it is almost certainly a pasted key,
/// and a key in the config file is exactly what this indirection prevents.
fn resolve_auth(api_key_env: &str) -> anyhow::Result<Option<String>> {
    let name = api_key_env.trim();
    if name.is_empty() {
        return Ok(None);
    }
    let valid = !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    anyhow::ensure!(
        valid,
        "embeddings.api_key_env {name:?} is not an environment variable NAME — \
         configure the variable's name, never the key itself"
    );
    let key = std::env::var(name)
        .map_err(|_| anyhow::anyhow!("embeddings.api_key_env: environment variable {name} is not set"))?;
    anyhow::ensure!(!key.trim().is_empty(), "embeddings.api_key_env: environment variable {name} is empty");
    Ok(Some(format!("Bearer {}", key.trim())))
}

pub struct EmbedClient {
    agent: ureq::Agent,
    /// Full `…/embeddings` URL, endpoint trailing slashes normalized away.
    url: String,
    model: String,
    /// Ready `Bearer …` header value, resolved from `api_key_env` at
    /// construction (a missing variable fails fast, not mid-batch).
    auth: Option<String>,
    /// One interactive request's bound; the batch path scales it.
    base_timeout: std::time::Duration,
}

impl std::fmt::Debug for EmbedClient {
    // Manual impl: ureq::Agent's Debug is not part of our contract, and the
    // interesting identity is (url, model) anyway.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbedClient")
            .field("url", &self.url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl EmbedClient {
    /// Refuses to construct when embeddings are disabled or unconfigured —
    /// the ONE place that gates every semantic path, so the CLI error is a
    /// single clear line.
    pub fn new(cfg: &EmbeddingsConfig) -> anyhow::Result<EmbedClient> {
        anyhow::ensure!(cfg.enabled, "embeddings disabled (set embeddings.enabled=true in config)");
        anyhow::ensure!(
            !cfg.endpoint.is_empty() && !cfg.model.is_empty(),
            "embeddings not configured (embeddings.endpoint and embeddings.model required)"
        );
        check_endpoint(&cfg.endpoint, &cfg.allow_hosts)?;
        let auth = resolve_auth(&cfg.api_key_env)?;
        let base_timeout = std::time::Duration::from_secs(cfg.timeout_secs.max(1));
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .max_redirects(0) // with max_redirects_will_error (default true): any 3xx is an Err
            .timeout_global(Some(base_timeout))
            .http_status_as_error(false) // status checked explicitly below
            .build()
            .new_agent();
        Ok(EmbedClient {
            agent,
            url: format!("{}/embeddings", cfg.endpoint.trim_end_matches('/')),
            model: cfg.model.clone(),
            auth,
            base_timeout,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Interactive path (recall): one request under the tight base bound.
    pub fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.embed_with_timeout(texts, self.base_timeout)
    }

    /// Batch path (embed-index): the bound scales base + per-item, because a
    /// large batch on a slow backend is busy, not down.
    pub fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.embed_with_timeout(texts, batch_timeout(self.base_timeout, texts.len()))
    }

    /// Embeds a batch of texts; returns one vector per input, in input order
    /// (the response's `index` field is honored, not the array order).
    fn embed_with_timeout(
        &self,
        texts: &[&str],
        timeout: std::time::Duration,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        #[derive(serde::Deserialize)]
        struct Response {
            data: Vec<Row>,
        }
        #[derive(serde::Deserialize)]
        struct Row {
            index: Option<usize>,
            embedding: Vec<f32>,
        }
        let body = serde_json::json!({ "model": self.model, "input": texts }).to_string();
        let mut req = self
            .agent
            .post(&self.url)
            .config()
            .timeout_global(Some(timeout)) // per-request override of the agent bound
            .build()
            .header("content-type", "application/json");
        if let Some(auth) = &self.auth {
            req = req.header("authorization", auth);
        }
        let mut resp = req
            .send(body.as_bytes())
            .with_context(|| format!("POST {}", self.url))?;
        let status = resp.status();
        let text = resp
            .body_mut()
            .read_to_string()
            .with_context(|| format!("read response from {}", self.url))?;
        anyhow::ensure!(
            status.is_success(),
            "embeddings endpoint returned {status}: {}",
            snippet(&text)
        );
        let parsed: Response = serde_json::from_str(&text)
            .with_context(|| format!("unparseable embeddings response: {}", snippet(&text)))?;
        anyhow::ensure!(
            parsed.data.len() == texts.len(),
            "embeddings endpoint returned {} vector(s) for {} input(s)",
            parsed.data.len(),
            texts.len()
        );
        // Order by the response's `index` field where present — the spec does
        // not promise array order, and a mis-aligned vector silently attaches
        // the WRONG meaning to a block.
        let mut rows: Vec<(usize, Vec<f32>)> = parsed
            .data
            .into_iter()
            .enumerate()
            .map(|(pos, row)| (row.index.unwrap_or(pos), row.embedding))
            .collect();
        rows.sort_by_key(|(i, _)| *i);
        Ok(rows.into_iter().map(|(_, v)| v).collect())
    }
}

/// First ~120 chars of an error body — enough to diagnose, never a dump.
fn snippet(text: &str) -> String {
    text.chars().take(120).collect()
}

pub struct EmbedIndexReport {
    pub embedded: usize,
    pub total_blocks: usize,
}

/// Embeds every block lacking a vector, `batch` texts per request, committing
/// per batch — an interrupted run resumes exactly where it stopped (missing
/// vector row = not yet embedded). A changed model or dimension drops all
/// vectors first; the loop then naturally re-covers everything.
pub fn run(conn: &mut Connection, client: &EmbedClient, batch: usize) -> anyhow::Result<EmbedIndexReport> {
    let batch = batch.max(1);
    if index::ensure_embed_model(conn, client.model())? {
        println!("embedding model changed -> dropped existing vectors, re-embedding everything");
    }
    let mut embedded = 0usize;
    loop {
        let missing = index::blocks_without_vectors(conn, batch)?;
        if missing.is_empty() {
            break;
        }
        let texts: Vec<&str> = missing.iter().map(|(_, t)| t.as_str()).collect();
        let vectors = client.embed_batch(&texts)?;
        if let Some(first) = vectors.first()
            && index::ensure_embed_dim(conn, first.len())?
        {
            // The drop cleared other rows, not this fresh batch — the loop's
            // next blocks_without_vectors naturally re-covers everything.
            println!("embedding dimension changed -> dropped existing vectors, re-embedding everything");
        }
        // One transaction per batch: an interrupted run keeps every batch
        // committed so far, and the missing-row query resumes from there.
        let tx = conn.transaction()?;
        for ((block_id, _), vector) in missing.iter().zip(&vectors) {
            index::insert_vector(&tx, *block_id, vector)?;
        }
        tx.commit()?;
        embedded += missing.len();
        let (done, total) = index::vector_counts(conn)?;
        println!("embedded {done}/{total} blocks");
    }
    let (_, total_blocks) = index::vector_counts(conn)?;
    Ok(EmbedIndexReport { embedded, total_blocks })
}

/// CLI entry for `cfetch embed-index`.
pub fn embed_index_cmd(batch: usize) -> anyhow::Result<()> {
    let cfg = Config::load()?;
    let client = EmbedClient::new(&cfg.embeddings)?;
    let native = crate::paths::native_projects_root();
    let mut conn = index::ensure_fresh(&crate::paths::state_dir(), &cfg.brain_root, Some(&native))?;
    let report = run(&mut conn, &client, batch)?;
    println!(
        "embed-index complete: {} embedded this run, {} block(s) total",
        report.embedded, report.total_blocks
    );
    Ok(())
}

/// Semantic (`--semantic`) or hybrid (`--hybrid`) recall: embeds the query,
/// then ranks by cosine alone or fuses with the BM25 list via RRF.
pub fn semantic_hits(
    cfg: &Config,
    conn: &Connection,
    query: &str,
    limit: usize,
    hybrid: bool,
) -> anyhow::Result<Vec<index::Hit>> {
    let client = EmbedClient::new(&cfg.embeddings)
        .map_err(|e| anyhow::anyhow!("semantic recall unavailable: {e}"))?;
    let mut qv = client
        .embed(&[query])?
        .into_iter()
        .next()
        .context("embeddings endpoint returned no vector for the query")?;
    index::l2_normalize(&mut qv);
    if hybrid {
        index::hybrid_recall(conn, query, &qv, limit, cfg.recall.rrf_k)
    } else {
        index::semantic_recall(conn, &qv, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::sync::{Arc, Mutex};

    // ---- minimal canned-response HTTP server (std TcpListener only) ----

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Reads one HTTP request; returns (headers, body).
    fn read_request(s: &mut std::net::TcpStream) -> Option<(String, String)> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end = loop {
            let n = s.read(&mut tmp).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let content_length = headers
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        while buf.len() < header_end + content_length {
            let n = s.read(&mut tmp).ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let body =
            String::from_utf8_lossy(&buf[header_end..(header_end + content_length).min(buf.len())]).to_string();
        Some((headers, body))
    }

    fn http_response(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Spawns a one-connection-at-a-time server; `responder(request_no,
    /// request_body)` produces the FULL http response. Returns (base_url,
    /// recorded request bodies, recorded request headers).
    #[allow(clippy::type_complexity)]
    fn spawn_server<F>(responder: F) -> (String, Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<String>>>)
    where
        F: Fn(usize, &str) -> String + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let headers = Arc::new(Mutex::new(Vec::new()));
        let (recorded_bodies, recorded_headers) = (bodies.clone(), headers.clone());
        std::thread::spawn(move || {
            for (n, stream) in listener.incoming().enumerate() {
                let Ok(mut s) = stream else { break };
                let Some((hdrs, body)) = read_request(&mut s) else { continue };
                recorded_bodies.lock().unwrap().push(body.clone());
                recorded_headers.lock().unwrap().push(hdrs);
                let _ = s.write_all(responder(n, &body).as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}"), bodies, headers)
    }

    /// OpenAI-shaped response with one deterministic 2-d vector per input:
    /// input i (0-based, per request) -> [seed + i, 1.0]. Data rows are
    /// emitted in REVERSED order to prove the client honors `index`.
    fn canned_embeddings(body: &str, seed: f32) -> String {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let n = v["input"].as_array().unwrap().len();
        let rows: Vec<String> = (0..n)
            .rev()
            .map(|i| {
                format!(
                    r#"{{"object":"embedding","index":{i},"embedding":[{},1.0]}}"#,
                    seed + i as f32
                )
            })
            .collect();
        http_response(200, &format!(r#"{{"object":"list","data":[{}]}}"#, rows.join(",")))
    }

    fn client_for(url: &str) -> EmbedClient {
        EmbedClient::new(&EmbeddingsConfig {
            enabled: true,
            endpoint: url.to_string(),
            model: "test-model".to_string(),
            ..EmbeddingsConfig::default()
        })
        .unwrap()
    }

    // ---- SSRF guard ----

    #[test]
    fn ssrf_guard_matrix() {
        // https to a public host: ok
        assert!(check_endpoint("https://api.example.com/v1", &[]).is_ok());
        assert!(check_endpoint("https://api.example.com:8443/v1", &[]).is_ok());
        // loopback: ok even over plain http
        assert!(check_endpoint("http://127.0.0.1:8080", &[]).is_ok());
        assert!(check_endpoint("http://localhost:1234/v1", &[]).is_ok());
        assert!(check_endpoint("http://[::1]:8080", &[]).is_ok());
        assert!(check_endpoint("https://127.0.0.1:8080/v1", &[]).is_ok());
        // http to a public host: refused (bearer tokens in cleartext)
        assert!(check_endpoint("http://example.com/v1", &[]).is_err());
        // private / link-local / metadata ranges: refused on BOTH schemes
        assert!(check_endpoint("http://10.0.0.5:11434", &[]).is_err());
        assert!(check_endpoint("https://10.0.0.5", &[]).is_err());
        assert!(check_endpoint("http://192.168.1.10:8080", &[]).is_err());
        // CGNAT refused by default; allow_hosts exempts the exact host from the
        // RANGE refusal only — https stays mandatory even for exempted hosts.
        assert!(check_endpoint("https://100.64.0.7", &[]).is_err());
        let allow = vec!["100.64.0.7".to_string()];
        assert!(check_endpoint("https://100.64.0.7", &allow).is_ok());
        assert!(check_endpoint("http://100.64.0.7", &allow).is_err());
        assert!(check_endpoint("https://192.168.1.1", &[]).is_err());
        assert!(check_endpoint("https://172.16.0.1", &[]).is_err());
        assert!(check_endpoint("http://169.254.169.254/latest/meta-data", &[]).is_err());
        assert!(check_endpoint("https://169.254.169.254/latest/meta-data", &[]).is_err());
        assert!(check_endpoint("https://[fe80::1]/v1", &[]).is_err());
        assert!(check_endpoint("https://[fd00::1]/v1", &[]).is_err());
        // scheme and shape violations
        assert!(check_endpoint("ftp://example.com", &[]).is_err());
        assert!(check_endpoint("file:///etc/passwd", &[]).is_err());
        assert!(check_endpoint("not a url", &[]).is_err());
        assert!(check_endpoint("https://", &[]).is_err());
        // userinfo could smuggle credentials into logs / confuse host parsing
        assert!(check_endpoint("https://user:pass@example.com/v1", &[]).is_err());
    }

    #[test]
    fn disabled_or_unconfigured_client_is_refused_with_one_line() {
        let err = EmbedClient::new(&EmbeddingsConfig::default()).unwrap_err();
        assert!(err.to_string().contains("disabled"), "got: {err}");
        assert!(!err.to_string().contains('\n'), "one-line error contract");
        let err = EmbedClient::new(&EmbeddingsConfig {
            enabled: true,
            endpoint: String::new(),
            model: "m".into(),
            ..EmbeddingsConfig::default()
        })
        .unwrap_err();
        assert!(!err.to_string().is_empty());
        let err = EmbedClient::new(&EmbeddingsConfig {
            enabled: true,
            endpoint: "http://127.0.0.1:1".into(),
            model: String::new(),
            ..EmbeddingsConfig::default()
        })
        .unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn semantic_recall_unavailable_without_config_is_one_line() {
        let state = tempfile::tempdir().unwrap();
        let conn = index::open(state.path()).unwrap();
        let err = semantic_hits(&Config::default(), &conn, "query", 5, false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("semantic recall unavailable"), "got: {msg}");
        assert!(!msg.contains('\n'));
        let err = semantic_hits(&Config::default(), &conn, "query", 5, true).unwrap_err();
        assert!(err.to_string().contains("semantic recall unavailable"), "--hybrid gated too");
    }

    // ---- client wire behavior ----

    #[test]
    fn embed_posts_openai_shape_and_orders_by_index() {
        let (url, bodies, _) = spawn_server(|_, body| canned_embeddings(body, 10.0));
        let client = client_for(&url);
        let out = client.embed(&["alpha", "beta"]).unwrap();
        // response rows arrive reversed; `index` must restore input order
        assert_eq!(out, vec![vec![10.0, 1.0], vec![11.0, 1.0]]);
        let sent: serde_json::Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(sent["model"], "test-model");
        assert_eq!(sent["input"], serde_json::json!(["alpha", "beta"]));
    }

    #[test]
    fn redirects_are_refused() {
        let (url, _, _) = spawn_server(|_, _| {
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:9/elsewhere\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                .to_string()
        });
        let client = client_for(&url);
        assert!(client.embed(&["x"]).is_err(), "a 3xx must be an error, never followed");
    }

    #[test]
    fn non_2xx_status_is_an_error() {
        let (url, _, _) = spawn_server(|_, _| http_response(500, r#"{"error":"boom"}"#));
        let client = client_for(&url);
        let err = client.embed(&["x"]).unwrap_err();
        assert!(err.to_string().contains("500"), "status surfaced: {err}");
    }

    #[test]
    fn short_response_is_an_error() {
        // 1 vector for 2 inputs must not silently mis-align block ids
        let (url, _, _) = spawn_server(|_, body| {
            let _ = body;
            http_response(200, r#"{"object":"list","data":[{"index":0,"embedding":[1.0,0.0]}]}"#)
        });
        let client = client_for(&url);
        assert!(client.embed(&["a", "b"]).is_err());
    }

    // ---- auth header ----

    #[test]
    fn api_key_env_sets_bearer_header() {
        // The config carries the env var's NAME; the key comes from the
        // process environment at construction time.
        // SAFETY: test-only unique variable name; no reader depends on it.
        unsafe { std::env::set_var("CFETCH_TEST_EMBED_KEY", "sk-cfetch-test") };
        let (url, _, headers) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let client = EmbedClient::new(&EmbeddingsConfig {
            enabled: true,
            endpoint: url,
            model: "test-model".into(),
            api_key_env: "CFETCH_TEST_EMBED_KEY".into(),
            ..EmbeddingsConfig::default()
        })
        .unwrap();
        client.embed(&["x"]).unwrap();
        let sent = headers.lock().unwrap()[0].to_ascii_lowercase();
        assert!(sent.contains("authorization: bearer sk-cfetch-test"), "got headers:\n{sent}");
    }

    #[test]
    fn no_api_key_env_means_no_auth_header() {
        let (url, _, headers) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let client = client_for(&url);
        client.embed(&["x"]).unwrap();
        let sent = headers.lock().unwrap()[0].to_ascii_lowercase();
        assert!(!sent.contains("authorization:"), "no auth configured, none sent:\n{sent}");
    }

    #[test]
    fn unset_or_literal_api_key_env_is_refused() {
        let base = EmbeddingsConfig {
            enabled: true,
            endpoint: "http://127.0.0.1:1".into(),
            model: "m".into(),
            ..EmbeddingsConfig::default()
        };
        // configured NAME whose variable is absent from the environment
        let err = EmbedClient::new(&EmbeddingsConfig {
            api_key_env: "CFETCH_TEST_DEFINITELY_UNSET_VAR".into(),
            ..base.clone()
        })
        .unwrap_err();
        assert!(err.to_string().contains("not set"), "got: {err}");
        // a literal key pasted where the NAME belongs must be refused loudly
        let err = EmbedClient::new(&EmbeddingsConfig {
            api_key_env: "sk-abc123.secret-key".into(),
            ..base
        })
        .unwrap_err();
        assert!(err.to_string().contains("never the key itself"), "got: {err}");
    }

    // ---- timeouts ----

    #[test]
    fn batch_timeout_scales_base_plus_per_item() {
        let base = std::time::Duration::from_secs(10);
        assert_eq!(batch_timeout(base, 0), base);
        assert_eq!(batch_timeout(base, 1), base + PER_ITEM);
        assert_eq!(batch_timeout(base, 64), base + PER_ITEM * 64);
    }

    #[test]
    fn interactive_timeout_stays_tight() {
        // Server answers after 2 s; the interactive bound is 1 s.
        let (url, _, _) = spawn_server(|_, body| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            canned_embeddings(body, 0.0)
        });
        let client = EmbedClient::new(&EmbeddingsConfig {
            enabled: true,
            endpoint: url,
            model: "test-model".into(),
            timeout_secs: 1,
            ..EmbeddingsConfig::default()
        })
        .unwrap();
        assert!(client.embed(&["x"]).is_err(), "recall must not wait for a slow backend");
    }

    #[test]
    fn batch_timeout_outlives_a_backend_too_slow_for_interactive() {
        // Same 2 s server, same 1 s base — but the batch path's bound is
        // base + per-item (1 + 2 = 3 s for one input), so it succeeds.
        let (url, _, _) = spawn_server(|_, body| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            canned_embeddings(body, 0.0)
        });
        let client = EmbedClient::new(&EmbeddingsConfig {
            enabled: true,
            endpoint: url,
            model: "test-model".into(),
            timeout_secs: 1,
            ..EmbeddingsConfig::default()
        })
        .unwrap();
        let out = client.embed_batch(&["x"]).unwrap();
        assert_eq!(out.len(), 1);
    }

    // ---- embed-index over a real (temp) index ----

    fn five_block_index() -> (tempfile::TempDir, tempfile::TempDir, Connection) {
        let brain = tempfile::tempdir().unwrap();
        let p = brain.path().join("knowledge/a.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "- one\n- two\n- three\n- four\n- five\n").unwrap();
        let state = tempfile::tempdir().unwrap();
        let mut conn = index::open(state.path()).unwrap();
        index::scan(&mut conn, brain.path(), None).unwrap();
        (brain, state, conn)
    }

    #[test]
    fn embed_index_embeds_all_blocks_in_batches() {
        let (_brain, _state, mut conn) = five_block_index();
        let (url, bodies, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let client = client_for(&url);
        let report = run(&mut conn, &client, 2).unwrap();
        assert_eq!(report.embedded, 5);
        assert_eq!(report.total_blocks, 5);
        assert_eq!(index::vector_counts(&conn).unwrap(), (5, 5));
        let sizes: Vec<usize> = bodies
            .lock()
            .unwrap()
            .iter()
            .map(|b| serde_json::from_str::<serde_json::Value>(b).unwrap()["input"].as_array().unwrap().len())
            .collect();
        assert_eq!(sizes, vec![2, 2, 1], "batched requests");
        // meta recorded for future model/dim gating
        let model: String = conn
            .query_row("SELECT value FROM meta WHERE key='embed_model'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(model, "test-model");
        let dim: String = conn
            .query_row("SELECT value FROM meta WHERE key='embed_dim'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(dim, "2");
    }

    #[test]
    fn embed_index_is_resumable_after_midway_failure() {
        let (_brain, _state, mut conn) = five_block_index();
        // First server: batch 1 succeeds, batch 2 fails -> run() errors, but
        // the first batch's vectors are already committed.
        let (url_a, _, _) = spawn_server(|n, body| {
            if n == 0 { canned_embeddings(body, 0.0) } else { http_response(500, "{}") }
        });
        let client_a = client_for(&url_a);
        assert!(run(&mut conn, &client_a, 2).is_err());
        assert_eq!(index::vector_counts(&conn).unwrap().0, 2, "committed batch survives the failure");

        // Second server: healthy. Only the 3 missing blocks get embedded.
        let (url_b, bodies_b, _) = spawn_server(|_, body| canned_embeddings(body, 100.0));
        let client_b = client_for(&url_b);
        let report = run(&mut conn, &client_b, 2).unwrap();
        assert_eq!(report.embedded, 3, "resume embeds only what is missing");
        assert_eq!(index::vector_counts(&conn).unwrap(), (5, 5));
        let inputs_b: Vec<String> = bodies_b
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| {
                serde_json::from_str::<serde_json::Value>(b).unwrap()["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(inputs_b, vec!["- three", "- four", "- five"], "already-embedded blocks not re-sent");
    }

    #[test]
    fn embed_index_after_model_change_re_embeds_everything() {
        let (_brain, _state, mut conn) = five_block_index();
        let (url, bodies, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let client = client_for(&url);
        run(&mut conn, &client, 8).unwrap();
        assert_eq!(index::vector_counts(&conn).unwrap(), (5, 5));
        // Same endpoint, different model name -> full drop + re-embed.
        let client2 = EmbedClient::new(&EmbeddingsConfig {
            enabled: true,
            endpoint: url.clone(),
            model: "other-model".to_string(),
            ..EmbeddingsConfig::default()
        })
        .unwrap();
        let report = run(&mut conn, &client2, 8).unwrap();
        assert_eq!(report.embedded, 5, "model change drops vectors; all re-embedded");
        assert_eq!(bodies.lock().unwrap().len(), 2);
    }
}
