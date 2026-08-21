//! Configuration. Deep-merged over defaults so a partial file never erases the
//! rest; unknown fields are ignored so old binaries read new configs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths;

/// The outermost legal ring. 0-6 is the whole trust scale; a rule naming
/// anything beyond it is a typo, not a policy, and is refused at load.
pub const MAX_RING: u8 = 6;

/// Ring a path gets when NO rule matches it. It is the curated-knowledge
/// ring: an unclassified brain file is knowledge, never policy and never
/// quarantine. A list that wants a different answer says so with a catch-all
/// rule (empty prefix) of its own.
pub const UNMATCHED_RING: u8 = 3;

/// Path prefixes that can never be indexed, whatever the config says:
/// credentials, session exhaust, and git internals. This is the boundary the
/// whole trust model rests on, so it is compiled in rather than configured —
/// `exclude_prefixes` can only ADD to it.
const HARD_EXCLUDE_PREFIXES: &[&str] = &["mind/secrets/", "logs/"];

/// One entry of the path -> ring taxonomy.
///
/// Matching is deliberately ORDERED and first-match-wins rather than
/// longest-prefix: the file reads top to bottom in the order it fires, so a
/// specific rule is simply placed above a general one and no one has to
/// count characters to predict the outcome.
///
/// A prefix ending in `/` matches the whole subtree. A prefix without a
/// trailing slash matches that exact path only, so `AGENT.md` never captures
/// `AGENT.md.bak`. The empty prefix matches everything — that is how a list
/// declares its own catch-all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingRule {
    pub prefix: String,
    pub ring: u8,
}

impl RingRule {
    fn matches(&self, rel: &str) -> bool {
        if self.prefix.is_empty() {
            true
        } else if self.prefix.ends_with('/') {
            rel.starts_with(&self.prefix)
        } else {
            rel == self.prefix
        }
    }
}

/// The complete path taxonomy one scan works from: the ordered ring rules and
/// the operator's own exclusions. Everything that needs to know "what ring is
/// this path, and may it be indexed at all" takes one of these, so the
/// mapping exists in exactly one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingRules {
    pub rules: Vec<RingRule>,
    pub exclude_prefixes: Vec<String>,
}

impl Default for RingRules {
    fn default() -> Self {
        RingRules {
            rules: default_ring_rules(),
            exclude_prefixes: default_exclude_prefixes(),
        }
    }
}

/// True when `rel` is `prefix` itself or anything beneath it. Written against
/// path components, not raw strings, so `drafts` never swallows `draftsman`;
/// a trailing slash on the prefix is accepted and meaningless.
fn under_prefix(rel: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return false; // an empty exclusion would exclude the world by accident
    }
    rel == prefix || rel.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
}

/// Git internals ANYWHERE in the tree — a brain may hold repo clones, each
/// with its own `.git`.
fn in_git_dir(rel: &str) -> bool {
    rel == ".git" || rel.starts_with(".git/") || rel.ends_with("/.git") || rel.contains("/.git/")
}

impl RingRules {
    /// Ring for a brain-root-relative path: the first matching rule, else
    /// [`UNMATCHED_RING`]. Pure — no filesystem, no frontmatter (a `ring: N`
    /// key in the file overrides this afterwards, at scan time).
    pub fn ring_for(&self, rel: &str) -> u8 {
        self.rules.iter().find(|r| r.matches(rel)).map(|r| r.ring).unwrap_or(UNMATCHED_RING)
    }

    /// Whether a path must never enter any index or capture record. The hard
    /// boundary first, the operator's own exclusions after it.
    pub fn excluded(&self, rel: &str) -> bool {
        let rel = rel.trim_end_matches('/');
        in_git_dir(rel)
            || HARD_EXCLUDE_PREFIXES.iter().any(|p| under_prefix(rel, p))
            || self.exclude_prefixes.iter().any(|p| under_prefix(rel, p))
    }

    /// Directory form: true when nothing under `rel` can ever be indexed, so
    /// a watcher can skip the whole subtree. It is the SAME predicate as
    /// [`RingRules::excluded`] — the two used to be hand-maintained copies of
    /// one list, which is exactly how a watcher drifts from its indexer.
    pub fn excluded_dir(&self, rel: &str) -> bool {
        self.excluded(rel)
    }
}

/// The shipped taxonomy. Opinionated (it describes a conventional agent brain
/// tree) but not baked in: replacing `ring_rules` in the config replaces it
/// whole.
fn default_ring_rules() -> Vec<RingRule> {
    vec![
        // The tree's own entry points: what every agent reads first.
        RingRule { prefix: "AGENT.md".into(), ring: 1 },
        RingRule { prefix: "README.md".into(), ring: 1 },
        // Exactly the memory index — a topic file named e.g. OLD-MEMORY.md
        // must not inherit ring 1 by suffix accident.
        RingRule { prefix: "mind/memories/MEMORY.md".into(), ring: 1 },
        // Distilled behavioral memories.
        RingRule { prefix: "mind/memories/".into(), ring: 2 },
        // Working state: queues and task notes.
        RingRule { prefix: "todo/".into(), ring: 4 },
        // Everything else is curated knowledge; see UNMATCHED_RING.
    ]
}

/// Shipped exclusions ON TOP of the hard boundary: repo clones belong to the
/// code index, and the archive is retired knowledge nobody should recall by
/// accident. Both are conventions, so both are configurable.
fn default_exclude_prefixes() -> Vec<String> {
    vec!["projects/".into(), "knowledge/archive/".into()]
}

/// Where a resident entry may be injected. An entry with no scope at all is
/// injected everywhere — which is what every config written before scopes
/// existed keeps meaning.
///
/// `hosts` and `repos` are ORed: an entry listing both lands on any listed
/// host AND in any listed repo, rather than only where the two coincide.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// Machine names this entry belongs to.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Working-directory names (the repo, as the agent sees it) this entry
    /// belongs to.
    #[serde(default)]
    pub repos: Vec<String>,
    /// Inject regardless of host and repo — an explicit "everywhere" that
    /// survives someone later adding one host to the list.
    #[serde(default)]
    pub always: bool,
}

impl Scope {
    /// No condition named at all.
    pub fn is_unscoped(&self) -> bool {
        self.hosts.is_empty() && self.repos.is_empty()
    }

    /// Whether this entry belongs in a session on `host`, working in `repo`.
    pub fn matches(&self, host: &str, repo: Option<&str>) -> bool {
        if self.always || self.is_unscoped() {
            return true;
        }
        if self.hosts.iter().any(|h| host_matches(host, h)) {
            return true;
        }
        repo.is_some_and(|r| self.repos.iter().any(|want| want == r))
    }
}

/// A `hosts` entry matches the machine's node name exactly, or its first
/// dot-label — so `build-box` still matches a `build-box.example.net` that
/// reports its FQDN.
fn host_matches(host: &str, want: &str) -> bool {
    host == want || host.split('.').next() == Some(want)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResidentEntry {
    pub path: PathBuf,
    /// Privilege ring of the file's statements (0 = invariants, 1 = policy,
    /// 2 = distilled behavior). Rings 0-1 may be injected everywhere; ring 2
    /// only under a scope, because behavior memories are SELECTIVE by
    /// definition — injecting the lot of them unconditionally is the
    /// "resident set" this design replaced. Rings 3+ are recall-only and are
    /// refused at load time.
    pub ring: u8,
    /// When this entry reaches a session. Absent = every session.
    #[serde(default)]
    pub scope: Scope,
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
    /// The path -> ring taxonomy, in order; the FIRST matching rule wins.
    /// Replacing this list replaces the shipped one whole.
    #[serde(default = "default_ring_rules")]
    pub ring_rules: Vec<RingRule>,
    /// Extra path prefixes to keep out of the index, ON TOP of the hard
    /// boundary (secrets, logs, `.git`), which no config can lift.
    #[serde(default = "default_exclude_prefixes")]
    pub exclude_prefixes: Vec<String>,
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
            resident: vec![ResidentEntry {
                path: PathBuf::from("AGENT.md"),
                ring: 1,
                scope: Scope::default(),
            }],
            code_roots: Vec::new(),
            budget_chars: default_budget_chars(),
            ledger_max_sessions: default_ledger_max_sessions(),
            capture: CaptureConfig::default(),
            embeddings: EmbeddingsConfig::default(),
            recall: RecallConfig::default(),
            governance: GovernanceConfig::default(),
            serve: ServeConfig::default(),
            client: ClientConfig::default(),
            ring_rules: default_ring_rules(),
            exclude_prefixes: default_exclude_prefixes(),
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
            match r.ring {
                0 | 1 => {}
                // Ring 2 is injectable, but only as policy: a scope, and not
                // an `always` that would smuggle back the unconditional set.
                2 if !r.scope.is_unscoped() && !r.scope.always => {}
                2 => anyhow::bail!(
                    "resident entry {} is ring 2: behavior memories are injected selectively —                      give it a scope (hosts/repos), or leave it to recall",
                    r.path.display()
                ),
                n => anyhow::bail!(
                    "resident entry {} has ring {n}; rings 0-1 may be resident anywhere, ring 2                      only under a scope, rings 3+ never",
                    r.path.display()
                ),
            }
        }
        if cfg.serve.enabled && cfg.client.serving.is_some() {
            anyhow::bail!(
                "serve.enabled and client.serving are mutually exclusive: serving needs a local \
                 index, a none-tier client must open none"
            );
        }
        for r in &cfg.ring_rules {
            if r.ring > MAX_RING {
                anyhow::bail!(
                    "ring rule {:?} names ring {}; rings run 0-{MAX_RING}",
                    r.prefix,
                    r.ring
                );
            }
        }
        if cfg.serve.bind.is_some() && cfg.serve.token_file.is_none() {
            anyhow::bail!(
                "serve.bind requires serve.token_file: the TCP listener is bearer-token gated, \
                 an open listener is unconfigurable"
            );
        }
        Ok(cfg)
    }

    /// The taxonomy this config describes, as one value to hand the indexer.
    pub fn rings(&self) -> RingRules {
        RingRules {
            rules: self.ring_rules.clone(),
            exclude_prefixes: self.exclude_prefixes.clone(),
        }
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
    fn resident_ring_above_two_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"resident": [{"path": "x.md", "ring": 3}]}"#).unwrap();
        assert!(Config::load_from(&p).is_err());
        std::fs::write(&p, r#"{"resident": [{"path": "x.md", "ring": 5}]}"#).unwrap();
        assert!(Config::load_from(&p).is_err());
    }

    #[test]
    fn ring_two_is_resident_only_under_a_scope() {
        // The selective-injection contract, enforced at load: a behavior
        // memory reaches the sessions it is FOR, or it stays in recall.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");

        std::fs::write(
            &p,
            r#"{"resident": [{"path": "b.md", "ring": 2, "scope": {"repos": ["widget"]}}]}"#,
        )
        .unwrap();
        assert!(Config::load_from(&p).is_ok(), "a scoped ring-2 entry is the point");

        std::fs::write(&p, r#"{"resident": [{"path": "b.md", "ring": 2}]}"#).unwrap();
        let err = Config::load_from(&p).unwrap_err().to_string();
        assert!(err.contains("selectively"), "the message must say why: {err}");

        std::fs::write(
            &p,
            r#"{"resident": [{"path": "b.md", "ring": 2, "scope": {"always": true}}]}"#,
        )
        .unwrap();
        assert!(
            Config::load_from(&p).is_err(),
            "`always` must not smuggle an unconditional ring-2 set back in"
        );
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
    fn shipped_ring_rules_reproduce_the_historical_mapping() {
        // Regression lock: the shipped default must assign exactly what the
        // hardcoded taxonomy assigned before it became configuration.
        let r = RingRules::default();
        assert_eq!(r.ring_for("AGENT.md"), 1);
        assert_eq!(r.ring_for("README.md"), 1);
        assert_eq!(r.ring_for("mind/memories/MEMORY.md"), 1);
        assert_eq!(r.ring_for("mind/memories/feedback_x.md"), 2);
        assert_eq!(r.ring_for("todo/active/task/STATUS.md"), 4);
        assert_eq!(r.ring_for("knowledge/hosts/example/storage.md"), 3);
        // A non-slash prefix is an EXACT path, never a string prefix.
        assert_eq!(r.ring_for("AGENT.md.bak"), 3);
        assert_eq!(r.ring_for("docs/AGENT.md"), 3);
    }

    #[test]
    fn first_matching_rule_wins_in_list_order() {
        let rules = RingRules {
            rules: vec![
                RingRule { prefix: "notes/pinned/".into(), ring: 1 },
                RingRule { prefix: "notes/".into(), ring: 4 },
                RingRule { prefix: String::new(), ring: 2 },
            ],
            exclude_prefixes: Vec::new(),
        };
        assert_eq!(rules.ring_for("notes/pinned/a.md"), 1, "the specific rule stands first");
        assert_eq!(rules.ring_for("notes/b.md"), 4);
        assert_eq!(rules.ring_for("anything/else.md"), 2, "empty prefix is the catch-all");
    }

    #[test]
    fn unmatched_paths_land_on_the_documented_fallback_ring() {
        let rules = RingRules { rules: Vec::new(), exclude_prefixes: Vec::new() };
        assert_eq!(rules.ring_for("whatever.md"), UNMATCHED_RING);
        assert_eq!(UNMATCHED_RING, 3);
    }

    #[test]
    fn custom_ring_rules_remap_a_fabricated_tree() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"ring_rules": [
                 {"prefix": "laws.md", "ring": 0},
                 {"prefix": "handbook/", "ring": 1},
                 {"prefix": "scratch/", "ring": 5},
                 {"prefix": "", "ring": 4}
               ]}"#,
        )
        .unwrap();
        let r = Config::load_from(&p).unwrap().rings();
        assert_eq!(r.ring_for("laws.md"), 0);
        assert_eq!(r.ring_for("handbook/style.md"), 1);
        assert_eq!(r.ring_for("scratch/dump.md"), 5);
        assert_eq!(r.ring_for("anything.md"), 4);
        // The shipped tree names carry no special meaning any more.
        assert_eq!(r.ring_for("AGENT.md"), 4);
        assert_eq!(r.ring_for("todo/x.md"), 4);
    }

    #[test]
    fn ring_above_six_is_refused_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"ring_rules": [{"prefix": "x/", "ring": 7}]}"#).unwrap();
        let err = Config::load_from(&p).unwrap_err().to_string();
        assert!(err.contains("ring 7"), "the message must name the bad ring: {err}");
        std::fs::write(&p, r#"{"ring_rules": [{"prefix": "x/", "ring": 6}]}"#).unwrap();
        assert!(Config::load_from(&p).is_ok(), "6 is the outermost legal ring");
    }

    #[test]
    fn hard_exclusions_survive_an_override_attempt() {
        // An operator may ADD exclusions; the credential/exhaust/git boundary
        // is not theirs to remove.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"exclude_prefixes": [], "ring_rules": [
                 {"prefix": "mind/secrets/", "ring": 0},
                 {"prefix": "logs/", "ring": 0},
                 {"prefix": ".git/", "ring": 0}
               ]}"#,
        )
        .unwrap();
        let r = Config::load_from(&p).unwrap().rings();
        assert!(r.excluded("mind/secrets/tokens.yml"));
        assert!(r.excluded("logs/session.log"));
        assert!(r.excluded(".git/config"));
        assert!(r.excluded("nested/repo/.git/config"), "a nested .git is git internals too");
        assert!(r.excluded_dir("mind/secrets"));
        assert!(r.excluded_dir("logs"));
        // What IS configurable actually became configurable.
        assert!(!r.excluded("projects/repo/notes.md"), "an emptied list stops excluding projects/");
        assert!(!r.excluded("knowledge/archive/old.md"));
    }

    #[test]
    fn shipped_exclusions_keep_projects_and_archive_out() {
        let r = RingRules::default();
        assert!(r.excluded("projects/repo/README.md"));
        assert!(r.excluded("knowledge/archive/old.md"));
        assert!(r.excluded_dir("projects"));
        assert!(r.excluded_dir("knowledge/archive"));
        assert!(!r.excluded("knowledge/live.md"));
        assert!(!r.excluded(".gitignore"), "a dotfile is not the .git directory");
    }

    #[test]
    fn operator_exclusions_add_to_the_hard_ones() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"exclude_prefixes": ["drafts"]}"#).unwrap();
        let r = Config::load_from(&p).unwrap().rings();
        assert!(r.excluded("drafts/x.md"), "a slashless prefix still means the subtree");
        assert!(r.excluded_dir("drafts"));
        assert!(!r.excluded("draftsman.md"), "prefix matching stops at the path separator");
        assert!(r.excluded("mind/secrets/x.yml"), "hard exclusions never depend on the list");
    }

    #[test]
    fn scope_defaults_to_everywhere_and_matches_host_or_repo() {
        let unscoped = Scope::default();
        assert!(unscoped.matches("any-host", Some("any-repo")));
        assert!(unscoped.matches("any-host", None));

        let by_host = Scope { hosts: vec!["build-box".into()], ..Scope::default() };
        assert!(by_host.matches("build-box", None));
        assert!(by_host.matches("build-box.example.net", None), "the first label matches too");
        assert!(!by_host.matches("laptop", Some("build-box")), "a host rule is not a repo rule");

        let by_repo = Scope { repos: vec!["widget".into()], ..Scope::default() };
        assert!(by_repo.matches("laptop", Some("widget")));
        assert!(!by_repo.matches("laptop", Some("gadget")));
        assert!(!by_repo.matches("laptop", None), "no cwd, no repo match");

        let both = Scope { hosts: vec!["build-box".into()], repos: vec!["widget".into()], always: false };
        assert!(both.matches("build-box", Some("gadget")), "hosts and repos are ORed");
        assert!(both.matches("laptop", Some("widget")));
        assert!(!both.matches("laptop", Some("gadget")));

        let always = Scope { hosts: vec!["build-box".into()], repos: Vec::new(), always: true };
        assert!(always.matches("laptop", None), "always wins over a narrower list");
    }

    #[test]
    fn resident_entry_scope_parses_and_defaults_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"resident": [
                 {"path": "a.md", "ring": 1},
                 {"path": "b.md", "ring": 1, "scope": {"repos": ["widget"]}},
                 {"path": "c.md", "ring": 0, "scope": {"hosts": ["build-box"], "always": true}}
               ]}"#,
        )
        .unwrap();
        let cfg = Config::load_from(&p).unwrap();
        assert!(cfg.resident[0].scope.matches("any", None), "absent scope = everywhere");
        assert_eq!(cfg.resident[1].scope.repos, vec!["widget".to_string()]);
        assert!(cfg.resident[2].scope.always);
    }

    #[test]
    fn corrupt_config_is_an_error_not_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, "{ nope").unwrap();
        assert!(Config::load_from(&p).is_err());
    }
}
