//! Supervision boundary for a package-local inference adapter.
//!
//! cfetch owns process identity and lifetime. The target adapter owns only its
//! native runtime. A random bearer travels over stdin, the child binds an
//! ephemeral loopback port, and EOF tells it that its parent is gone.

use std::io::{BufRead as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context as _;
use sha2::Digest as _;

const READY_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_READY_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterEndpoint {
    pub base_url: String,
    pub authorization: String,
}

#[derive(Debug, Clone)]
pub struct AdapterLaunch {
    pub binary: PathBuf,
    pub sha256: String,
    pub package_manifest: PathBuf,
    pub package_manifest_sha256: String,
    pub ordered_scope_ids: Vec<String>,
}

struct RunningAdapter {
    child: Child,
    /// Kept open for the whole child lifetime. The adapter treats EOF as a
    /// parent-death signal and shuts down instead of becoming an orphan.
    _stdin: ChildStdin,
    endpoint: AdapterEndpoint,
}

pub struct AdapterSupervisor {
    launch: AdapterLaunch,
    running: Option<RunningAdapter>,
    restarted_after_crash: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyLine {
    schema_version: u32,
    url: String,
    scope_ids: Vec<String>,
}

impl AdapterSupervisor {
    pub fn new(launch: AdapterLaunch) -> anyhow::Result<Self> {
        validate_launch(&launch)?;
        Ok(Self {
            launch,
            running: None,
            restarted_after_crash: false,
        })
    }

    /// Starts lazily and returns the current authenticated loopback endpoint.
    /// A child found dead is restarted once for this supervisor's lifetime.
    pub fn endpoint(&mut self) -> anyhow::Result<AdapterEndpoint> {
        if self.child_exited()? {
            self.restart_once("package-local adapter exited")?;
        }
        if self.running.is_none() {
            self.running = Some(spawn_adapter(&self.launch)?);
        }
        Ok(self
            .running
            .as_ref()
            .expect("adapter was started")
            .endpoint
            .clone())
    }

    /// Transport failure is retryable only when the supervised child really
    /// crashed. A malformed signed response or a live process that refuses a
    /// request is not converted into broad fallback by this layer.
    pub fn restart_after_transport_failure(&mut self) -> anyhow::Result<AdapterEndpoint> {
        anyhow::ensure!(
            self.child_exited()?,
            "package-local adapter transport failed while its supervised process remained alive"
        );
        self.restart_once("package-local adapter crashed during a request")?;
        self.endpoint()
    }

    fn child_exited(&mut self) -> anyhow::Result<bool> {
        let Some(running) = self.running.as_mut() else {
            return Ok(false);
        };
        match running.child.try_wait().context("inspect package-local adapter")? {
            Some(_) => {
                self.running.take();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn restart_once(&mut self, reason: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.restarted_after_crash,
            "{reason}; the one supervised restart was already consumed"
        );
        self.restarted_after_crash = true;
        self.stop();
        self.running = Some(spawn_adapter(&self.launch)?);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(mut running) = self.running.take() {
            // Closing stdin first gives a well-behaved adapter a clean exit.
            drop(running._stdin);
            if running.child.try_wait().ok().flatten().is_none() {
                let _ = running.child.kill();
            }
            let _ = running.child.wait();
        }
    }
}

impl Drop for AdapterSupervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn validate_launch(launch: &AdapterLaunch) -> anyhow::Result<()> {
    validate_sha256(&launch.sha256, "package-local adapter digest")?;
    validate_sha256(
        &launch.package_manifest_sha256,
        "package-local root manifest digest",
    )?;
    anyhow::ensure!(
        !launch.ordered_scope_ids.is_empty(),
        "package-local adapter needs at least one admitted scope"
    );
    anyhow::ensure!(
        launch.package_manifest.file_name().and_then(|name| name.to_str())
            == Some("package-manifest.json")
            && launch.binary.parent() == launch.package_manifest.parent(),
        "package-local root manifest must be package-manifest.json beside the adapter"
    );
    validate_regular_file(&launch.binary, "package-local adapter")?;
    validate_regular_file(
        &launch.package_manifest,
        "package-local root manifest",
    )?;
    let actual = file_sha256(&launch.binary)?;
    anyhow::ensure!(
        actual == launch.sha256,
        "package-local adapter digest mismatch: package plan requires {}, found {actual}",
        launch.sha256
    );
    let actual_manifest = file_sha256(&launch.package_manifest)?;
    anyhow::ensure!(
        actual_manifest == launch.package_manifest_sha256,
        "package-local root manifest digest mismatch: package plan requires {}, found {actual_manifest}",
        launch.package_manifest_sha256
    );
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} must be 64 lowercase hexadecimal characters"
    );
    Ok(())
}

fn validate_regular_file(path: &Path, label: &str) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "{label} must be a regular non-symlink file"
    );
    Ok(())
}

fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let mut input = std::fs::File::open(path)
        .with_context(|| format!("open package-local adapter {}", path.display()))?;
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .with_context(|| format!("hash package-local adapter {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn spawn_adapter(launch: &AdapterLaunch) -> anyhow::Result<RunningAdapter> {
    // Re-check immediately before execution. Construction and first use may
    // be separated by a long-running daemon's lifetime.
    validate_launch(launch)?;
    let bearer = hex(&iroh::SecretKey::generate().to_bytes());
    let mut child = std::process::Command::new(&launch.binary)
        .args([
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--auth-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start package-local adapter {}", launch.binary.display()))?;
    let mut stdin = child.stdin.take().context("adapter stdin was not piped")?;
    let secret_line = serde_json::to_vec(&serde_json::json!({"bearer": bearer}))?;
    stdin.write_all(&secret_line)?;
    stdin.write_all(b"\n")?;
    stdin.flush()?;

    let stdout = child.stdout.take().context("adapter stdout was not piped")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut bytes = Vec::new();
        let result = reader
            .by_ref()
            .take((MAX_READY_BYTES + 1) as u64)
            .read_until(b'\n', &mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let ready_bytes = match receiver.recv_timeout(READY_TIMEOUT) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            terminate(&mut child);
            return Err(error).context("read package-local adapter readiness");
        }
        Err(_) => {
            terminate(&mut child);
            anyhow::bail!("timed out waiting for package-local adapter readiness");
        }
    };
    if ready_bytes.len() > MAX_READY_BYTES || !ready_bytes.ends_with(b"\n") {
        terminate(&mut child);
        anyhow::bail!("package-local adapter readiness line is missing or exceeds its bound");
    }
    let ready: ReadyLine = match serde_json::from_slice(&ready_bytes) {
        Ok(ready) => ready,
        Err(error) => {
            terminate(&mut child);
            return Err(error).context("parse package-local adapter readiness");
        }
    };
    if let Err(error) = validate_ready_line(&ready, &launch.ordered_scope_ids) {
        terminate(&mut child);
        return Err(error);
    }
    Ok(RunningAdapter {
        child,
        _stdin: stdin,
        endpoint: AdapterEndpoint {
            base_url: ready.url,
            authorization: format!("Bearer {bearer}"),
        },
    })
}

fn validate_ready_line(ready: &ReadyLine, expected_scopes: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(ready.schema_version == 1, "unsupported adapter readiness schema");
    anyhow::ensure!(
        ready.scope_ids == expected_scopes,
        "package-local adapter readiness scopes do not exactly match its package plan"
    );
    let rest = ready
        .url
        .strip_prefix("http://127.0.0.1:")
        .context("package-local adapter must advertise IPv4 loopback HTTP")?;
    let (port, path) = rest
        .split_once('/')
        .context("package-local adapter readiness URL has no path")?;
    let port: u16 = port.parse().context("package-local adapter readiness port is invalid")?;
    anyhow::ensure!(port != 0 && path == "v1", "package-local adapter readiness URL must end in /v1");
    Ok(())
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_is_exact_and_loopback_only() {
        let scopes = vec!["npu-scope".to_string(), "gpu-scope".to_string()];
        validate_ready_line(
            &ReadyLine {
                schema_version: 1,
                url: "http://127.0.0.1:43123/v1".into(),
                scope_ids: scopes.clone(),
            },
            &scopes,
        )
        .unwrap();
        for url in [
            "https://127.0.0.1:43123/v1",
            "http://localhost:43123/v1",
            "http://127.0.0.1:0/v1",
            "http://127.0.0.1:43123/other",
        ] {
            let ready = ReadyLine {
                schema_version: 1,
                url: url.into(),
                scope_ids: scopes.clone(),
            };
            assert!(validate_ready_line(&ready, &scopes).is_err(), "accepted {url}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn supervisor_hashes_starts_authenticates_and_stops_a_sibling() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let adapter = directory.path().join("fake-adapter");
        let package_manifest = directory.path().join("package-manifest.json");
        std::fs::write(
            &adapter,
            "#!/bin/sh\nIFS= read -r secret\nprintf '%s\\n' '{\"schema_version\":1,\"url\":\"http://127.0.0.1:43123/v1\",\"scope_ids\":[\"npu-scope\",\"gpu-scope\",\"cpu-scope\"]}'\ncat >/dev/null\n",
        )
        .unwrap();
        std::fs::write(&package_manifest, "{\"schema_version\":1}\n").unwrap();
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o700)).unwrap();
        let launch = AdapterLaunch {
            sha256: file_sha256(&adapter).unwrap(),
            binary: adapter,
            package_manifest_sha256: file_sha256(&package_manifest).unwrap(),
            package_manifest,
            ordered_scope_ids: vec![
                "npu-scope".into(),
                "gpu-scope".into(),
                "cpu-scope".into(),
            ],
        };
        let mut supervisor = AdapterSupervisor::new(launch).unwrap();
        let endpoint = supervisor.endpoint().unwrap();
        assert_eq!(endpoint.base_url, "http://127.0.0.1:43123/v1");
        assert!(endpoint.authorization.starts_with("Bearer "));
        assert_eq!(endpoint.authorization.len(), "Bearer ".len() + 64);
        drop(supervisor);
    }

    #[cfg(unix)]
    #[test]
    fn supervisor_refuses_root_manifest_drift_before_launch() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let adapter = directory.path().join("fake-adapter");
        let package_manifest = directory.path().join("package-manifest.json");
        std::fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&package_manifest, "{\"schema_version\":1}\n").unwrap();
        let launch = AdapterLaunch {
            binary: adapter.clone(),
            sha256: file_sha256(&adapter).unwrap(),
            package_manifest: package_manifest.clone(),
            package_manifest_sha256: "0".repeat(64),
            ordered_scope_ids: vec!["npu-scope".into()],
        };
        let error = AdapterSupervisor::new(launch).err().unwrap().to_string();
        assert!(error.contains("root manifest digest mismatch"), "{error}");
    }
}
