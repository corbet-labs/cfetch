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
use crate::vectors;

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
pub(crate) fn resolve_auth(api_key_env: &str, field: &str) -> anyhow::Result<Option<String>> {
    let name = api_key_env.trim();
    if name.is_empty() {
        return Ok(None);
    }
    let valid = !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    anyhow::ensure!(
        valid,
        "{field}.api_key_env {name:?} is not an environment variable NAME — \
         configure the variable's name, never the key itself"
    );
    let key = std::env::var(name)
        .map_err(|_| anyhow::anyhow!("{field}.api_key_env: environment variable {name} is not set"))?;
    anyhow::ensure!(!key.trim().is_empty(), "{field}.api_key_env: environment variable {name} is empty");
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
    /// Stored/queried vector width. Sent to the endpoint as `dimensions`
    /// (Matryoshka truncation server-side) AND enforced on the response, so
    /// an endpoint that ignores the parameter still yields exactly this
    /// width. 0 = take whatever the model returns.
    dimensions: usize,
    /// Instruction prepended to a query, never to a document (see
    /// [`crate::config::EmbeddingsConfig::query_prefix`]).
    query_prefix: String,
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
        let auth = resolve_auth(&cfg.api_key_env, "embeddings")?;
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
            dimensions: cfg.dimensions,
            query_prefix: cfg.query_prefix.clone(),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Brings one response vector to the configured width. A model that
    /// returns MORE is truncated to the Matryoshka prefix and re-normalized
    /// (the endpoint either ignored `dimensions` or does not support it); a
    /// model that returns FEWER is an error, never a silently padded or
    /// silently narrower vector — the operator asked for a width this model
    /// cannot produce and has to hear it.
    fn fit(&self, mut v: Vec<f32>) -> anyhow::Result<Vec<f32>> {
        if self.dimensions == 0 {
            return Ok(v);
        }
        anyhow::ensure!(
            v.len() >= self.dimensions,
            "model {} returned {}-d vectors but embeddings.dimensions asks for {} \
             (set embeddings.dimensions to {} or fewer, or configure a wider model)",
            self.model,
            v.len(),
            self.dimensions,
            v.len()
        );
        if v.len() > self.dimensions {
            // A Matryoshka prefix is no longer a unit vector: re-normalize,
            // or every truncated vector would score short against the query.
            v.truncate(self.dimensions);
            index::l2_normalize(&mut v);
        }
        Ok(v)
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

    /// Embeds ONE query, with the configured instruction prefix applied.
    ///
    /// This is the only path that prefixes anything. `embed`/`embed_batch`
    /// stay raw, because what they produce is stored and shared, and a
    /// document embedded with an instruction is not the same artifact.
    pub fn embed_query(&self, query: &str) -> anyhow::Result<Vec<f32>> {
        let text = if self.query_prefix.is_empty() {
            query.to_string()
        } else {
            format!("{}{query}", self.query_prefix)
        };
        self.embed(&[text.as_str()])?
            .into_iter()
            .next()
            .context("endpoint returned no vector for the query")
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
        let mut request = serde_json::json!({ "model": self.model, "input": texts });
        if self.dimensions > 0 {
            // Modern embedders are Matryoshka-trained: asking for the width
            // we store saves the endpoint work and the wire the bytes. An
            // endpoint that ignores the field is handled by `fit`.
            request["dimensions"] = serde_json::json!(self.dimensions);
        }
        let body = request.to_string();
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
        rows.into_iter().map(|(_, v)| self.fit(v)).collect()
    }
}

/// First ~120 chars of an error body — enough to diagnose, never a dump.
pub(crate) fn snippet(text: &str) -> String {
    text.chars().take(120).collect()
}

pub struct EmbedIndexReport {
    /// Vectors derived from the endpoint in this run.
    pub embedded: usize,
    /// Vectors taken from the shared store instead of being re-derived.
    pub imported: usize,
    pub total_blocks: usize,
}

/// Derives the vectors this storage group is still missing.
///
/// COMPUTE-ONCE: the shared store in the tree is the record. The run first
/// takes everything it already holds (`hydrate`), then embeds only content
/// hashes no host has derived yet — writing each batch to the SHARED store
/// before caching it locally, so an interrupted run leaves its work where the
/// next run (on any host) will find it. A missing vector is the whole
/// resumability contract; nothing else is remembered between runs.
pub fn run(
    conn: &mut Connection,
    client: &EmbedClient,
    batch: usize,
    store: &mut vectors::VectorStore,
) -> anyhow::Result<EmbedIndexReport> {
    let batch = batch.max(1);
    let spec = store.spec().clone();
    anyhow::ensure!(
        spec.model == client.model() && spec.dim == client.dimensions(),
        "the shared store is {} at {} dimensions, the client is {} at {} — one artifact, one spec",
        spec.model,
        spec.dim,
        client.model(),
        client.dimensions()
    );
    if index::ensure_vector_spec(conn, &spec)? {
        println!("embedding spec changed -> local vector cache dropped, re-filling for {}", spec.model);
    }
    let imported = vectors::hydrate(conn, store)?;
    if imported > 0 {
        println!("imported {imported} vector(s) from the shared store (already derived by this group)");
    }
    let mut embedded = 0usize;
    let mut pending = index::hashes_without_vectors(conn, &spec, batch)?;
    if !pending.is_empty() {
        // The write lock is taken only when there IS something to derive: a
        // host that only reads never needs the store to be writable.
        let mut writer = store.begin_write()?;
        loop {
            let texts: Vec<&str> = pending.iter().map(|(_, t)| t.as_str()).collect();
            let vectors = client.embed_batch(&texts)?;
            // Record first, cache second: the shared artifact is what the
            // group keeps, the local row is a convenience.
            for ((hash, _), vector) in pending.iter().zip(&vectors) {
                writer.put(hash, vector)?;
            }
            writer.flush()?;
            let tx = conn.transaction()?;
            for ((hash, _), vector) in pending.iter().zip(&vectors) {
                index::insert_vector(&tx, hash, &spec, vector)?;
            }
            tx.commit()?;
            embedded += pending.len();
            let (done, total) = index::vector_coverage(conn, &spec)?;
            println!("embedded {done}/{total} blocks");
            pending = index::hashes_without_vectors(conn, &spec, batch)?;
            if pending.is_empty() {
                break;
            }
        }
    }
    let (_, total_blocks) = index::vector_coverage(conn, &spec)?;
    Ok(EmbedIndexReport { embedded, imported, total_blocks })
}

/// CLI entry for `cfetch embed-index`.
pub fn embed_index_cmd(batch: usize) -> anyhow::Result<()> {
    let cfg = Config::load()?;
    let client = EmbedClient::new(&cfg.embeddings)?;
    let spec = cfg.embeddings.spec();
    let mut store = vectors::VectorStore::open(&cfg.brain_root, &spec)?;
    let native = crate::paths::native_projects_root();
    let mut conn = index::ensure_fresh(&crate::paths::state_dir(), &cfg.brain_root, Some(&native), &cfg.rings())?;
    let report = run(&mut conn, &client, batch, &mut store)?;
    println!(
        "embed-index complete: {} embedded this run, {} imported from the shared store, {} block(s) total",
        report.embedded, report.imported, report.total_blocks
    );
    println!(
        "shared vector store: {} ({} artifact(s), {} at {} dimensions)",
        crate::paths::shared_vector_dir(&cfg.brain_root).display(),
        store.len(),
        spec.precision.as_str(),
        spec.dim
    );
    Ok(())
}

/// The `cfetch status` line for semantic recall. Coverage leads, because a
/// half-embedded index is exactly the state that would otherwise degrade
/// every hybrid query without anyone noticing.
pub fn coverage_status_line(
    spec: &crate::config::VectorSpec,
    embedded: usize,
    total: usize,
    shared: usize,
) -> String {
    let health = if total == 0 {
        "index is empty".to_string()
    } else if embedded >= total {
        "complete".to_string()
    } else {
        format!("run cfetch embed-index for the remaining {}", total - embedded)
    };
    format!(
        "semantic: {embedded}/{total} blocks embedded — {health}\n  \
         {} at {} dims ({}), shared store holds {shared} artifact(s)",
        spec.model,
        spec.dim,
        spec.precision.as_str()
    )
}

/// A semantic/hybrid answer, plus what was WRONG with it. `note` is the
/// project's anti-silent-degradation contract in one field: a memory system
/// that quietly returns worse answers is the failure this exists to prevent,
/// so partial or absent vector coverage travels with the result and every
/// caller surfaces it.
#[derive(Debug)]
pub struct SemanticRecall {
    pub hits: Vec<index::Hit>,
    pub note: Option<String>,
}

/// The coverage line, or None when nothing is degraded.
fn coverage_note(embedded: usize, total: usize) -> Option<String> {
    if total == 0 || embedded >= total {
        return None;
    }
    Some(if embedded == 0 {
        format!(
            "semantic: 0/{total} blocks embedded — answering lexically only; run cfetch embed-index"
        )
    } else {
        format!(
            "semantic: {embedded}/{total} blocks embedded — {} block(s) cannot be reached \
             semantically; run cfetch embed-index",
            total - embedded
        )
    })
}

/// Semantic (`--semantic`) or hybrid (`--hybrid`) recall: embeds the query,
/// then ranks by cosine alone or fuses with the BM25 list via RRF.
pub fn semantic_hits(
    cfg: &Config,
    conn: &Connection,
    query: &str,
    limit: usize,
    hybrid: bool,
) -> anyhow::Result<SemanticRecall> {
    let client = EmbedClient::new(&cfg.embeddings)
        .map_err(|e| anyhow::anyhow!("semantic recall unavailable: {e}"))?;
    let spec = cfg.embeddings.spec();
    // The shared store is the record: take whatever this group already
    // derived before judging our own coverage.
    let store = vectors::VectorStore::open(&cfg.brain_root, &spec)?;
    vectors::hydrate(conn, &store)?;
    let (embedded, total) = index::vector_coverage(conn, &spec)?;
    let note = coverage_note(embedded, total);
    if embedded == 0 {
        // Nothing to rank against. Answer lexically — but say so; a silently
        // lexical "hybrid" is the degradation this project bans.
        return Ok(SemanticRecall { hits: index::recall(conn, query, limit)?, note });
    }
    let embedded_query = client.embed_query(query);
    let mut qv = match embedded_query {
        Ok(qv) => qv,
        Err(e) => {
            // Configured but not answering: the vectors are here, the thing
            // that would place the QUERY among them is not. Degrade to
            // lexical — and say exactly that, on one line, with the reason.
            // (An UNCONFIGURED endpoint is a different thing and still
            // errors above: you cannot degrade a feature you never enabled.)
            let reason = format!("semantic: query embedding failed ({e:#}) — answering lexically");
            let reason = reason.replace('\n', " ");
            return Ok(SemanticRecall {
                hits: index::recall(conn, query, limit)?,
                note: Some(match note {
                    Some(coverage) => format!("{coverage}; {reason}"),
                    None => reason,
                }),
            });
        }
    };
    index::l2_normalize(&mut qv);
    let hits = if hybrid {
        index::hybrid_recall(conn, &spec, query, &qv, limit, cfg.recall.rrf_k)?
    } else {
        index::semantic_recall(conn, &spec, &qv, limit)?
    };
    Ok(SemanticRecall { hits, note })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testhttp::{http_response, spawn_server};

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
            dimensions: 2,
            ..EmbeddingsConfig::default()
        })
        .unwrap()
    }

    /// Response whose vectors are `width` long regardless of the requested
    /// `dimensions` — the endpoint that ignores the parameter.
    fn canned_width(body: &str, width: usize) -> String {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let n = v["input"].as_array().unwrap().len();
        let rows: Vec<String> = (0..n)
            .map(|i| {
                let comps: Vec<String> = (0..width).map(|d| format!("{}", (i + d + 1) as f32)).collect();
                format!(r#"{{"object":"embedding","index":{i},"embedding":[{}]}}"#, comps.join(","))
            })
            .collect();
        http_response(200, &format!(r#"{{"object":"list","data":[{}]}}"#, rows.join(",")))
    }

    /// Response honoring the requested `dimensions` (Matryoshka-style
    /// truncation at the endpoint), falling back to `native` when absent.
    fn canned_honoring_dimensions(body: &str, native: usize) -> String {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let width = v["dimensions"].as_u64().map(|d| d as usize).unwrap_or(native);
        canned_width(body, width.min(native))
    }

    fn spec_for(dim: usize) -> crate::config::VectorSpec {
        crate::config::VectorSpec {
            model: "test-model".into(),
            dim,
            precision: crate::config::Precision::F16,
        }
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
    fn the_instruction_prefixes_queries_and_never_documents() {
        // Asymmetric retrieval: the instruction belongs on the query side.
        // A document embedded with it would not be the same artifact as the
        // one every other host derived, so `embed`/`embed_batch` stay raw.
        let (url, bodies, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let mut cfg = EmbeddingsConfig {
            enabled: true,
            endpoint: url.clone(),
            model: "test-model".into(),
            dimensions: 2,
            ..EmbeddingsConfig::default()
        };
        cfg.query_prefix = "Instruct: find it\nQuery: ".into();
        let client = EmbedClient::new(&cfg).unwrap();

        client.embed_query("what is it").unwrap();
        client.embed_batch(&["a stored block"]).unwrap();
        let sent = bodies.lock().unwrap();
        let q: serde_json::Value = serde_json::from_str(&sent[0]).unwrap();
        assert_eq!(q["input"], serde_json::json!(["Instruct: find it\nQuery: what is it"]));
        let d: serde_json::Value = serde_json::from_str(&sent[1]).unwrap();
        assert_eq!(d["input"], serde_json::json!(["a stored block"]), "documents stay raw");
    }

    #[test]
    fn an_empty_prefix_leaves_the_query_untouched() {
        let (url, bodies, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let client = client_for(&url);
        client.embed_query("plain").unwrap();
        let q: serde_json::Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(q["input"], serde_json::json!(["plain"]));
    }

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
            dimensions: 2,
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
            dimensions: 2,
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
            dimensions: 2,
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
        index::scan(&mut conn, brain.path(), None, &crate::config::RingRules::default()).unwrap();
        (brain, state, conn)
    }

    /// A shared store over the given brain tree, at the 2-d test spec.
    fn store_for(brain: &std::path::Path) -> crate::vectors::VectorStore {
        crate::vectors::VectorStore::open(brain, &spec_for(2)).unwrap()
    }

    #[test]
    fn embed_index_embeds_all_blocks_in_batches() {
        let (brain, _state, mut conn) = five_block_index();
        let (url, bodies, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let client = client_for(&url);
        let mut store = store_for(brain.path());
        let report = run(&mut conn, &client, 2, &mut store).unwrap();
        assert_eq!(report.embedded, 5);
        assert_eq!(report.imported, 0);
        assert_eq!(report.total_blocks, 5);
        assert_eq!(index::vector_coverage(&conn, &spec_for(2)).unwrap(), (5, 5));
        assert_eq!(store.len(), 5, "the shared tree is the record, not index.db");
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
        let (brain, _state, mut conn) = five_block_index();
        // First server: batch 1 succeeds, batch 2 fails -> run() errors, but
        // the first batch's vectors are already committed.
        let (url_a, _, _) = spawn_server(|n, body| {
            if n == 0 { canned_embeddings(body, 0.0) } else { http_response(500, "{}") }
        });
        let client_a = client_for(&url_a);
        let mut store = store_for(brain.path());
        assert!(run(&mut conn, &client_a, 2, &mut store).is_err());
        assert_eq!(
            index::vector_coverage(&conn, &spec_for(2)).unwrap().0,
            2,
            "committed batch survives the failure"
        );
        assert_eq!(store.len(), 2, "and it survives in the SHARED store, not only locally");

        // Second server: healthy. Only the 3 missing blocks get embedded.
        let (url_b, bodies_b, _) = spawn_server(|_, body| canned_embeddings(body, 100.0));
        let client_b = client_for(&url_b);
        let report = run(&mut conn, &client_b, 2, &mut store).unwrap();
        assert_eq!(report.embedded, 3, "resume embeds only what is missing");
        assert_eq!(index::vector_coverage(&conn, &spec_for(2)).unwrap(), (5, 5));
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
        let (brain, _state, mut conn) = five_block_index();
        let (url, bodies, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let client = client_for(&url);
        let mut store = store_for(brain.path());
        run(&mut conn, &client, 8, &mut store).unwrap();
        assert_eq!(index::vector_coverage(&conn, &spec_for(2)).unwrap(), (5, 5));
        // Same endpoint, different model name -> a different artifact: full
        // drop of the old cache, a NEW shared store, everything re-embedded.
        let cfg2 = EmbeddingsConfig {
            enabled: true,
            endpoint: url.clone(),
            model: "other-model".to_string(),
            dimensions: 2,
            ..EmbeddingsConfig::default()
        };
        let client2 = EmbedClient::new(&cfg2).unwrap();
        let mut store2 = crate::vectors::VectorStore::open(brain.path(), &cfg2.spec()).unwrap();
        let report = run(&mut conn, &client2, 8, &mut store2).unwrap();
        assert_eq!(report.embedded, 5, "model change drops vectors; all re-embedded");
        assert_eq!(bodies.lock().unwrap().len(), 2);
        assert_eq!(store.len(), 5, "the old model's artifacts are untouched, not destroyed");
        assert_eq!(store2.len(), 5);
    }

    // ---- dimensions and width ----

    #[test]
    fn the_configured_width_is_requested_and_enforced_client_side() {
        // The endpoint here IGNORES `dimensions` and always answers 8-d, so
        // the client must truncate and re-normalize (Matryoshka prefix).
        let (url, bodies, _) = spawn_server(|_, body| canned_width(body, 8));
        let client = EmbedClient::new(&EmbeddingsConfig {
            enabled: true,
            endpoint: url,
            model: "test-model".into(),
            dimensions: 4,
            ..EmbeddingsConfig::default()
        })
        .unwrap();
        let out = client.embed(&["alpha"]).unwrap();
        let sent: serde_json::Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
        assert_eq!(sent["dimensions"], 4, "the request asks the endpoint for the width");
        assert_eq!(out[0].len(), 4, "an endpoint that ignores it is sliced client-side");
        let norm: f32 = out[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6, "re-normalized after slicing, got {norm}");
        // The prefix is the model's first 4 components, direction preserved.
        assert!(out[0][0] < out[0][1] && out[0][1] < out[0][2]);
    }

    #[test]
    fn an_endpoint_honoring_dimensions_is_taken_at_its_word() {
        let (url, _, _) = spawn_server(|_, body| canned_honoring_dimensions(body, 8));
        let client = EmbedClient::new(&EmbeddingsConfig {
            enabled: true,
            endpoint: url,
            model: "test-model".into(),
            dimensions: 3,
            ..EmbeddingsConfig::default()
        })
        .unwrap();
        assert_eq!(client.embed(&["alpha"]).unwrap()[0].len(), 3);
    }

    #[test]
    fn a_model_narrower_than_the_configured_width_is_loud() {
        // Never pad, never silently store a shorter vector: the operator
        // asked for a width this model cannot produce, and must hear it.
        let (url, _, _) = spawn_server(|_, body| canned_width(body, 2));
        let client = EmbedClient::new(&EmbeddingsConfig {
            enabled: true,
            endpoint: url,
            model: "test-model".into(),
            dimensions: 16,
            ..EmbeddingsConfig::default()
        })
        .unwrap();
        let err = client.embed(&["alpha"]).unwrap_err().to_string();
        assert!(err.contains("16") && err.contains('2'), "both widths named: {err}");
        assert!(err.contains("embeddings.dimensions"), "the fix is named: {err}");
    }

    #[test]
    fn dimensions_are_honored_end_to_end_into_the_shared_store() {
        let (brain, _state, mut conn) = five_block_index();
        let (url, _, _) = spawn_server(|_, body| canned_width(body, 8));
        let cfg = EmbeddingsConfig {
            enabled: true,
            endpoint: url,
            model: "test-model".into(),
            dimensions: 4,
            precision: crate::config::Precision::F16,
            ..EmbeddingsConfig::default()
        };
        let client = EmbedClient::new(&cfg).unwrap();
        let mut store = crate::vectors::VectorStore::open(brain.path(), &cfg.spec()).unwrap();
        run(&mut conn, &client, 8, &mut store).unwrap();
        let stored = store.get(&index::content_hash("- one")).unwrap().unwrap();
        assert_eq!(stored.len(), 4, "the artifact carries the configured width");
        let dim: i64 = conn.query_row("SELECT dim FROM vectors LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(dim, 4);
        let blob_len: i64 =
            conn.query_row("SELECT length(embedding) FROM vectors LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(blob_len, 8, "f16: 4 components in 8 bytes");
        let file = crate::paths::shared_vector_dir(brain.path()).join("test-model-4-f16.bin");
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 5 * 8, "5 blocks x 4 f16 components");
    }

    // ---- compute-once across hosts ----

    #[test]
    fn a_second_host_reads_the_shared_store_and_embeds_nothing() {
        let (brain, _state, mut conn) = five_block_index();
        let (url, _, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let mut store = store_for(brain.path());
        run(&mut conn, &client_for(&url), 8, &mut store).unwrap();
        assert_eq!(store.len(), 5);

        // Host B: same tree, its own empty state dir, and an endpoint that
        // fails every request. Derived-once means it must never be called.
        let state_b = tempfile::tempdir().unwrap();
        let mut conn_b = index::open(state_b.path()).unwrap();
        index::scan(&mut conn_b, brain.path(), None, &crate::config::RingRules::default()).unwrap();
        let (url_b, bodies_b, _) = spawn_server(|_, _| http_response(500, r#"{"error":"no"}"#));
        let mut store_b = store_for(brain.path());
        let report = run(&mut conn_b, &client_for(&url_b), 8, &mut store_b).unwrap();
        assert_eq!(report.embedded, 0, "nothing to derive: the group already derived it");
        assert_eq!(report.imported, 5, "the shared artifacts are read, not recomputed");
        assert!(bodies_b.lock().unwrap().is_empty(), "the endpoint was never called");
        assert_eq!(index::vector_coverage(&conn_b, &spec_for(2)).unwrap(), (5, 5));
    }

    // ---- no silent degradation ----

    fn semantic_config(brain: &std::path::Path, url: &str) -> Config {
        Config {
            brain_root: brain.to_path_buf(),
            embeddings: EmbeddingsConfig {
                enabled: true,
                endpoint: url.to_string(),
                model: "test-model".into(),
                dimensions: 2,
                ..EmbeddingsConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn the_status_line_states_coverage_and_the_artifact_spec() {
        let spec = spec_for(1024);
        let line = coverage_status_line(&spec, 0, 19478, 0);
        assert!(line.contains("0/19478 blocks embedded"), "got: {line}");
        assert!(line.contains("cfetch embed-index"), "got: {line}");
        let line = coverage_status_line(&spec, 19478, 19478, 19478);
        assert!(line.contains("complete"), "got: {line}");
        assert!(line.contains("1024 dims") && line.contains("f16"), "got: {line}");
        assert!(coverage_status_line(&spec, 0, 0, 0).contains("index is empty"));
    }

    #[test]
    fn zero_coverage_warns_with_the_numbers_and_answers_lexically() {
        let (brain, _state, conn) = five_block_index();
        let (url, bodies, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let cfg = semantic_config(brain.path(), &url);
        let out = semantic_hits(&cfg, &conn, "three", 5, true).unwrap();
        let note = out.note.expect("zero coverage must be reported, never hidden");
        assert!(note.contains("0/5"), "the numbers are in the warning: {note}");
        assert!(note.contains("cfetch embed-index"), "the fix is named: {note}");
        assert_eq!(out.hits.len(), 1, "the lexical answer is still delivered");
        assert!(out.hits[0].snippet.contains("three"));
        assert!(bodies.lock().unwrap().is_empty(), "no point embedding a query nothing can match");
        // --semantic degrades the same way: an answer plus the truth about it.
        let out = semantic_hits(&cfg, &conn, "three", 5, false).unwrap();
        assert!(out.note.is_some());
        assert_eq!(out.hits.len(), 1);
    }

    #[test]
    fn partial_coverage_warns_with_the_numbers_and_still_ranks() {
        let (brain, _state, conn) = five_block_index();
        let spec = spec_for(2);
        index::ensure_vector_spec(&conn, &spec).unwrap();
        for (hash, _) in index::hashes_without_vectors(&conn, &spec, 2).unwrap() {
            index::insert_vector(&conn, &hash, &spec, &[1.0, 0.0]).unwrap();
        }
        let (url, _, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let cfg = semantic_config(brain.path(), &url);
        let out = semantic_hits(&cfg, &conn, "one", 5, true).unwrap();
        let note = out.note.expect("partial coverage is degradation, and must be said");
        assert!(note.contains("2/5"), "got: {note}");
        assert!(!out.hits.is_empty());
    }

    #[test]
    fn an_unreachable_endpoint_degrades_to_labeled_lexical_never_to_nothing() {
        // Vectors are all there; the endpoint that would embed the QUERY is
        // down (a laptop off the VPN, a GPU box asleep). An agent must still
        // get its lexical answer — and must be told what it did not get.
        let (brain, _state, mut conn) = five_block_index();
        let (url, _, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let cfg = semantic_config(brain.path(), &url);
        let mut store = store_for(brain.path());
        run(&mut conn, &EmbedClient::new(&cfg.embeddings).unwrap(), 8, &mut store).unwrap();

        // Same config, an endpoint nothing listens on.
        let dead = Config {
            embeddings: EmbeddingsConfig { endpoint: "http://127.0.0.1:1".into(), ..cfg.embeddings },
            ..cfg
        };
        let out = semantic_hits(&dead, &conn, "three", 5, true).unwrap();
        let note = out.note.expect("a degraded answer must be labeled");
        assert!(note.contains("query embedding failed"), "got: {note}");
        assert!(note.contains("lexical"), "got: {note}");
        assert!(!note.contains('\n'), "one-line note contract: {note}");
        assert_eq!(out.hits.len(), 1, "the lexical answer still arrives");
    }

    #[test]
    fn full_coverage_is_silent() {
        let (brain, _state, mut conn) = five_block_index();
        let (url, _, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let cfg = semantic_config(brain.path(), &url);
        let mut store = store_for(brain.path());
        run(&mut conn, &EmbedClient::new(&cfg.embeddings).unwrap(), 8, &mut store).unwrap();
        let out = semantic_hits(&cfg, &conn, "one", 5, true).unwrap();
        assert!(out.note.is_none(), "no warning when nothing is degraded: {:?}", out.note);
        assert!(!out.hits.is_empty());
    }

    #[test]
    fn a_query_host_hydrates_from_the_shared_store_before_answering() {
        // The none-embed host: vectors exist in the tree, its index.db has
        // none. It must answer semantically without an embed run of its own.
        let (brain, _state, mut conn) = five_block_index();
        let (url, _, _) = spawn_server(|_, body| canned_embeddings(body, 0.0));
        let cfg = semantic_config(brain.path(), &url);
        let mut store = store_for(brain.path());
        run(&mut conn, &EmbedClient::new(&cfg.embeddings).unwrap(), 8, &mut store).unwrap();

        let state_b = tempfile::tempdir().unwrap();
        let mut conn_b = index::open(state_b.path()).unwrap();
        index::scan(&mut conn_b, brain.path(), None, &crate::config::RingRules::default()).unwrap();
        assert_eq!(index::vector_coverage(&conn_b, &spec_for(2)).unwrap(), (0, 5));
        let out = semantic_hits(&cfg, &conn_b, "one", 5, false).unwrap();
        assert!(out.note.is_none(), "the tree covered it: {:?}", out.note);
        assert!(!out.hits.is_empty());
        assert_eq!(index::vector_coverage(&conn_b, &spec_for(2)).unwrap(), (5, 5));
    }
}
