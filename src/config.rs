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

/// Embeddings backend: an OpenAI-compatible `/embeddings` endpoint (in this
/// house: llama-broker on the GPU platform). Disabled by default — semantic
/// recall is opt-in, and the client is NEVER called from hook entrypoints
/// (hooks must not spend network time on the interactive path).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Base URL; `/embeddings` is appended. Must be https or loopback —
    /// config is a file agents write, so the URL is SSRF-guarded at use.
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub model: String,
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
    fn corrupt_config_is_an_error_not_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, "{ nope").unwrap();
        assert!(Config::load_from(&p).is_err());
    }
}
