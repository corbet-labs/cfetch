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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "paths::default_brain_root")]
    pub brain_root: PathBuf,
    /// Files injected verbatim (budget-clipped) at session start, in order.
    /// Paths are relative to brain_root unless absolute.
    #[serde(default)]
    pub resident: Vec<ResidentEntry>,
    /// Hard cap on the injected digest, in characters.
    #[serde(default = "default_budget_chars")]
    pub budget_chars: usize,
    /// Sessions kept in the injection ledger (writer-side retention).
    #[serde(default = "default_ledger_max_sessions")]
    pub ledger_max_sessions: usize,
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
            budget_chars: default_budget_chars(),
            ledger_max_sessions: default_ledger_max_sessions(),
        }
    }
}

impl Config {
    /// Loads the config file; a missing file yields defaults, a corrupt file is
    /// an error the caller surfaces (a half-applied config is worse than none).
    pub fn load() -> anyhow::Result<Config> {
        let path = paths::config_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config::default());
            }
            Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
        };
        let mut cfg: Config = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        if cfg.resident.is_empty() {
            cfg.resident = Config::default().resident;
        }
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
}
