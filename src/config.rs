//! Configuration. Deep-merged over defaults so a partial file never erases the
//! rest; unknown fields are ignored so old binaries read new configs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResidentEntry {
    pub path: PathBuf,
    /// Privilege ring of the file's statements (0 = invariants, 1 = policy).
    /// Only rings 0-1 are resident; anything else is refused at load time.
    pub ring: u8,
}

/// Ring-6 exhaust capture (PostToolUse/Stop -> exhaust.db).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    /// On by default: exhaust is the raw material every later promotion is
    /// distilled from; an empty ring 6 starves rings 5 and 2.
    #[serde(default = "default_capture_enabled")]
    pub enabled: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig { enabled: default_capture_enabled() }
    }
}

fn default_capture_enabled() -> bool {
    true
}

/// Embeddings backend: any OpenAI-compatible `/embeddings` endpoint (a
/// llama.cpp server, LM Studio, vLLM, or a hosted API). Disabled by default — semantic
/// recall is opt-in, and the client is NEVER called from hook entrypoints
/// (hooks must not spend network time on the interactive path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Base URL; `/embeddings` is appended. Must be https or loopback —
    /// config is a file agents write, so the URL is SSRF-guarded at use.
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub model: String,
    /// Exact hostnames/IPs the operator exempts from the private-range
    /// refusal (mesh overlays, lab networks). Never exempts from https.
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    /// NAME of the environment variable holding the API key (e.g.
    /// "OPENAI_API_KEY") — never the key itself: config files travel in
    /// dotfiles and backups, keys must not. Empty = no Authorization header.
    #[serde(default)]
    pub api_key_env: String,
    /// Timeout for ONE interactive embeddings request, in seconds. This is
    /// the tight bound recall rides on; the embed-index batch path scales it
    /// per batched item (see embed::batch_timeout).
    #[serde(default = "default_embed_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        EmbeddingsConfig {
            enabled: false,
            endpoint: String::new(),
            model: String::new(),
            allow_hosts: Vec::new(),
            api_key_env: String::new(),
            timeout_secs: default_embed_timeout_secs(),
        }
    }
}

fn default_embed_timeout_secs() -> u64 {
    10
}

/// The governance loop: Stop-side reminder producers plus instruction-decay
/// cadence re-injection, all delivered at the next UserPromptSubmit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GovernanceConfig {
    #[serde(default = "default_governance_enabled")]
    pub enabled: bool,
    /// Every N-th post-tool event re-queues the top ring-0 rules (compliance
    /// decays measurably with generated output; see DESIGN's instruction-decay
    /// study). Zero switches the cadence off while keeping the Stop producers.
    #[serde(default = "default_reinject_every")]
    pub reinject_every: u32,
}

impl Default for GovernanceConfig {
    fn default() -> Self {
        GovernanceConfig {
            enabled: default_governance_enabled(),
            reinject_every: default_reinject_every(),
        }
    }
}

fn default_governance_enabled() -> bool {
    true
}

fn default_reinject_every() -> u32 {
    25
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallConfig {
    /// Reciprocal-rank-fusion constant for `recall --hybrid`. Small K weights
    /// top ranks heavily; K=2 measurably beat the textbook K=60 on a 22k-unit
    /// corpus (MRR 0.845 vs 0.627), so 2 is the default.
    #[serde(default = "default_rrf_k")]
    pub rrf_k: f64,
}

impl Default for RecallConfig {
    fn default() -> Self {
        RecallConfig { rrf_k: default_rrf_k() }
    }
}

fn default_rrf_k() -> f64 {
    2.0
}

/// Serving mode: this daemon owns its index lifecycle (watcher, generations,
/// drain barrier) and answers recall/find/expand for local clients — and for
/// remote ones when `bind` is set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServeConfig {
    #[serde(default)]
    pub enabled: bool,
    /// TCP listen address (e.g. "0.0.0.0:9737"). Absent = unix socket only.
    #[serde(default)]
    pub bind: Option<String>,
    /// Host id stamped on every response. Defaults to the machine hostname.
    #[serde(default)]
    pub origin: Option<String>,
    /// Bearer token file gating the TCP listener; must be mode 0600.
    /// Required when `bind` is set.
    #[serde(default)]
    pub token_file: Option<PathBuf>,
}

/// Remote serving host a none-tier client queries instead of any local index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientServingConfig {
    /// "host:port" of the serving daemon's TCP listener.
    pub addr: String,
    /// File holding the bearer token for that listener.
    pub token_file: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientConfig {
    /// When set, recall/find/expand route to this serving host and the host
    /// opens NO local index at all (none-tier by config). Unreachable is an
    /// explicit error — never a silent fallback to stale local data.
    #[serde(default)]
    pub serving: Option<ClientServingConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "paths::default_brain_root")]
    pub brain_root: PathBuf,
    /// Files injected verbatim (budget-clipped) at session start, in order.
    /// Paths are relative to brain_root unless absolute.
    #[serde(default)]
    pub resident: Vec<ResidentEntry>,
    /// Roots for the code index (`cfetch find`). Empty means the default:
    /// `<brain_root>/projects/github` — where the house repos live.
    #[serde(default)]
    pub code_roots: Vec<PathBuf>,
    /// Hard cap on the injected digest, in characters.
    #[serde(default = "default_budget_chars")]
    pub budget_chars: usize,
    /// Sessions kept in the injection ledger (writer-side retention).
    #[serde(default = "default_ledger_max_sessions")]
    pub ledger_max_sessions: usize,
    /// Ring-6 exhaust capture.
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    #[serde(default)]
    pub recall: RecallConfig,
    /// Governance loop (reminder queue + cadence rule refresh).
    #[serde(default)]
    pub governance: GovernanceConfig,
    /// Serving mode (storage host answering queries).
    #[serde(default)]
    pub serve: ServeConfig,
    /// Client routing (none-tier host querying a serving host).
    #[serde(default)]
    pub client: ClientConfig,
}

fn default_budget_chars() -> usize {
    6000
}

fn default_ledger_max_sessions() -> usize {
    200
}

impl Default for Config {
    fn default() -> Self {
        Config {
            brain_root: paths::default_brain_root(),
            resident: vec![ResidentEntry { path: PathBuf::from("AGENT.md"), ring: 1 }],
            code_roots: Vec::new(),
            budget_chars: default_budget_chars(),
            ledger_max_sessions: default_ledger_max_sessions(),
            capture: CaptureConfig::default(),
            embeddings: EmbeddingsConfig::default(),
            recall: RecallConfig::default(),
            governance: GovernanceConfig::default(),
            serve: ServeConfig::default(),
            client: ClientConfig::default(),
        }
    }
}

impl Config {
    /// Loads the config file; a missing file yields defaults, a corrupt file is
    /// an error the caller surfaces (a half-applied config is worse than none).
    pub fn load() -> anyhow::Result<Config> {
        Config::load_from(&paths::config_path())
    }

    pub fn load_from(path: &std::path::Path) -> anyhow::Result<Config> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config::default());
            }
            Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
        };
        // An explicitly empty `resident` list means "inject nothing" — the
        // default (AGENT.md) applies only when no config file exists at all.
        // On hosts where the harness already auto-loads the ring files,
        // injecting them again would double-pay the context budget.
        let cfg: Config = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        for r in &cfg.resident {
            if r.ring > 1 {
                anyhow::bail!(
                    "resident entry {} has ring {}; only rings 0-1 may be resident",
                    r.path.display(),
                    r.ring
                );
            }
        }
        if cfg.serve.enabled && cfg.client.serving.is_some() {
            anyhow::bail!(
                "serve.enabled and client.serving are mutually exclusive: serving needs a local \
                 index, a none-tier client must open none"
            );
        }
        if cfg.serve.bind.is_some() && cfg.serve.token_file.is_none() {
            anyhow::bail!(
                "serve.bind requires serve.token_file: the TCP listener is bearer-token gated, \
                 an open listener is unconfigurable"
            );
        }
        Ok(cfg)
    }

    pub fn resolve(&self, p: &std::path::Path) -> PathBuf {
        if p.is_absolute() { p.to_path_buf() } else { self.brain_root.join(p) }
    }

    pub fn effective_code_roots(&self) -> Vec<PathBuf> {
        if self.code_roots.is_empty() {
            vec![self.brain_root.join("projects/github")]
        } else {
            self.code_roots.iter().map(|p| self.resolve(p)).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("absent.json")).unwrap();
        assert_eq!(cfg.resident.len(), 1);
        assert_eq!(cfg.resident[0].path, PathBuf::from("AGENT.md"));
    }

    #[test]
    fn explicit_empty_resident_stays_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"resident": []}"#).unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert!(cfg.resident.is_empty(), "explicit [] must mean inject nothing");
    }

    #[test]
    fn resident_ring_above_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"resident": [{"path": "x.md", "ring": 3}]}"#).unwrap();
        assert!(Config::load_from(&p).is_err());
    }

    #[test]
    fn capture_defaults_on_and_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        assert!(Config::load_from(&p).unwrap().capture.enabled, "default: capture on");
        std::fs::write(&p, r#"{"resident": []}"#).unwrap();
        assert!(Config::load_from(&p).unwrap().capture.enabled, "partial file: capture on");
        std::fs::write(&p, r#"{"capture": {"enabled": false}}"#).unwrap();
        assert!(!Config::load_from(&p).unwrap().capture.enabled);
    }

    #[test]
    fn embeddings_default_disabled_and_rrf_k_default_2() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("absent.json")).unwrap();
        assert!(!cfg.embeddings.enabled);
        assert!(cfg.embeddings.endpoint.is_empty());
        assert!(cfg.embeddings.model.is_empty());
        assert_eq!(cfg.recall.rrf_k, 2.0);
    }

    #[test]
    fn embeddings_api_key_env_and_timeout_parse_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("absent.json")).unwrap();
        assert!(cfg.embeddings.api_key_env.is_empty(), "default: no auth");
        assert_eq!(cfg.embeddings.timeout_secs, 10, "default: tight interactive bound");
        assert_eq!(EmbeddingsConfig::default().timeout_secs, 10, "Default impl must agree with serde");
        let p = dir.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"embeddings": {"api_key_env": "MY_EMBED_KEY", "timeout_secs": 30}}"#,
        )
        .unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert_eq!(cfg.embeddings.api_key_env, "MY_EMBED_KEY");
        assert_eq!(cfg.embeddings.timeout_secs, 30);
        // partial block keeps the timeout default
        std::fs::write(&p, r#"{"embeddings": {"enabled": true}}"#).unwrap();
        assert_eq!(Config::load_from(&p).unwrap().embeddings.timeout_secs, 10);
    }

    #[test]
    fn embeddings_and_recall_blocks_parse() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"embeddings": {"enabled": true, "endpoint": "https://llm.example/v1", "model": "nomic"},
                "recall": {"rrf_k": 60}}"#,
        )
        .unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert!(cfg.embeddings.enabled);
        assert_eq!(cfg.embeddings.endpoint, "https://llm.example/v1");
        assert_eq!(cfg.embeddings.model, "nomic");
        assert_eq!(cfg.recall.rrf_k, 60.0);
    }

    #[test]
    fn governance_defaults_on_with_cadence_25() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("absent.json")).unwrap();
        assert!(cfg.governance.enabled, "default: governance on");
        assert_eq!(cfg.governance.reinject_every, 25);
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"resident": []}"#).unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert!(cfg.governance.enabled, "partial file: governance on");
        assert_eq!(cfg.governance.reinject_every, 25);
    }

    #[test]
    fn governance_block_parses_and_gates() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"governance": {"enabled": false}}"#).unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert!(!cfg.governance.enabled);
        assert_eq!(cfg.governance.reinject_every, 25, "partial block keeps the default cadence");
        std::fs::write(&p, r#"{"governance": {"reinject_every": 7}}"#).unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert!(cfg.governance.enabled);
        assert_eq!(cfg.governance.reinject_every, 7);
    }

    #[test]
    fn serve_and_client_default_off() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&dir.path().join("absent.json")).unwrap();
        assert!(!cfg.serve.enabled);
        assert!(cfg.serve.bind.is_none());
        assert!(cfg.serve.origin.is_none());
        assert!(cfg.serve.token_file.is_none());
        assert!(cfg.client.serving.is_none());
    }

    #[test]
    fn serve_and_client_blocks_parse() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"serve": {"enabled": true, "bind": "0.0.0.0:9737",
                          "origin": "storage-1", "token_file": "/tmp/t"}}"#,
        )
        .unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert!(cfg.serve.enabled);
        assert_eq!(cfg.serve.bind.as_deref(), Some("0.0.0.0:9737"));
        assert_eq!(cfg.serve.origin.as_deref(), Some("storage-1"));
        assert_eq!(cfg.serve.token_file, Some(PathBuf::from("/tmp/t")));

        std::fs::write(
            &p,
            r#"{"client": {"serving": {"addr": "storage-1.example:9737", "token_file": "/tmp/t"}}}"#,
        )
        .unwrap();
        let cfg = Config::load_from(&p).unwrap();
        let cs = cfg.client.serving.as_ref().unwrap();
        assert_eq!(cs.addr, "storage-1.example:9737");
        assert_eq!(cs.token_file, PathBuf::from("/tmp/t"));
    }

    #[test]
    fn serving_host_and_none_tier_client_are_mutually_exclusive() {
        // serve.enabled needs a local index; client.serving forbids one.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"serve": {"enabled": true},
                "client": {"serving": {"addr": "h:1", "token_file": "/tmp/t"}}}"#,
        )
        .unwrap();
        assert!(Config::load_from(&p).is_err());
    }

    #[test]
    fn serve_bind_requires_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"serve": {"enabled": true, "bind": "127.0.0.1:0"}}"#).unwrap();
        assert!(Config::load_from(&p).is_err(), "an open unauthenticated TCP listener must be unconfigurable");
    }

    #[test]
    fn corrupt_config_is_an_error_not_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, "{ nope").unwrap();
        assert!(Config::load_from(&p).is_err());
    }
}
