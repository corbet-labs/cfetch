//! S2 acceptance: two real daemon processes on one host, distinct identities
//! and state trees, redeem an invite and query only the granted slice.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sha2::Digest as _;

struct Host {
    _root: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
    config: PathBuf,
    brain: PathBuf,
}

impl Host {
    fn new(serve: bool, with_slice: bool) -> Self {
        Self::new_with_embeddings(serve, with_slice, None)
    }

    fn new_with_embeddings(serve: bool, with_slice: bool, endpoint: Option<&str>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let state = root.path().join("state");
        let runtime = root.path().join("run");
        let brain = root.path().join("brain");
        let config = root.path().join("config.json");
        for dir in [&home, &state, &runtime, &brain] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let slices = if with_slice {
            serde_json::json!([{"name": "shared", "prefixes": ["knowledge/shared"]}])
        } else {
            serde_json::json!([])
        };
        let embeddings = endpoint.map_or_else(
            || serde_json::json!({}),
            |endpoint| serde_json::json!({"enabled": true, "endpoint": endpoint}),
        );
        std::fs::write(
            &config,
            serde_json::to_vec(&serde_json::json!({
                "brain_root": brain,
                "slices": slices,
                "serve": {"enabled": serve, "origin": if serve { "iroh-origin" } else { "" }},
                "embeddings": embeddings,
            }))
            .unwrap(),
        )
        .unwrap();
        Self { _root: root, home, state, runtime, config, brain }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cfetch"));
        cmd.env("HOME", &self.home)
            .env("CFETCH_STATE_DIR", &self.state)
            .env("CFETCH_CONFIG", &self.config)
            .env("CFETCH_BRAIN", &self.brain)
            .env("XDG_RUNTIME_DIR", &self.runtime);
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "cfetch {args:?} failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    fn daemon(&self) -> Daemon<'_> {
        let mut child = self
            .command()
            .args(["daemon", "run"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..200 {
            let out = self.run(&["daemon", "status"]);
            if out.status.success()
                && String::from_utf8_lossy(&out.stdout).contains("daemon: running")
            {
                return Daemon { child, host: self };
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("daemon did not become ready");
    }
}

struct EmbeddingServer {
    endpoint: String,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EmbeddingServer {
    fn start() -> Self {
        let profile_output = Command::new(env!("CARGO_BIN_EXE_cfetch"))
            .args(["embedding-profile", "--json"])
            .output()
            .unwrap();
        assert!(profile_output.status.success());
        let profile: serde_json::Value = serde_json::from_slice(&profile_output.stdout).unwrap();
        let profile_manifest_sha256 = format!(
            "{:x}",
            sha2::Sha256::digest(serde_json::to_vec(&profile).unwrap())
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_calls = calls.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // macOS inherits O_NONBLOCK from the listening socket.
                        // The accepted connection is handled synchronously, so
                        // make that contract explicit before reading its body.
                        stream.set_nonblocking(false).unwrap();
                        use std::io::{Read as _, Write as _};
                        let mut raw = Vec::new();
                        let mut buf = [0u8; 4096];
                        let header_end = loop {
                            let n = stream.read(&mut buf).unwrap();
                            if n == 0 {
                                return;
                            }
                            raw.extend_from_slice(&buf[..n]);
                            if let Some(pos) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                                break pos + 4;
                            }
                        };
                        let headers = String::from_utf8_lossy(&raw[..header_end]);
                        let content_len: usize = headers
                            .lines()
                            .find_map(|line| {
                                line.split_once(':').and_then(|(name, value)| {
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse().unwrap())
                                })
                            })
                            .unwrap_or(0);
                        while raw.len() < header_end + content_len {
                            let n = stream.read(&mut buf).unwrap();
                            if n == 0 {
                                break;
                            }
                            raw.extend_from_slice(&buf[..n]);
                        }
                        let request: serde_json::Value =
                            serde_json::from_slice(&raw[header_end..header_end + content_len]).unwrap();
                        let count = request["input"].as_array().unwrap().len();
                        thread_calls.fetch_add(1, Ordering::SeqCst);
                        let mut embedding = vec![0.0; 768];
                        embedding[0] = 1.0;
                        let data: Vec<serde_json::Value> = (0..count)
                            .map(|index| serde_json::json!({"index": index, "embedding": embedding}))
                            .collect();
                        let body = serde_json::to_vec(&serde_json::json!({
                            "model": profile["model"],
                            "cfetch_profile": profile["profile_id"],
                            "cfetch_profile_manifest_sha256": profile_manifest_sha256,
                            "cfetch_model_revision": profile["model_revision"],
                            "cfetch_model_quantization": profile["model_quantization"],
                            "cfetch_model_artifact": profile["model_artifact_id"],
                            "data": data,
                        }))
                        .unwrap();
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .unwrap();
                        stream.write_all(&body).unwrap();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            endpoint: format!("http://{addr}"),
            calls,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for EmbeddingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

struct Daemon<'a> {
    child: Child,
    host: &'a Host,
}

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        let _ = self.host.run(&["daemon", "stop"]);
        for _ in 0..40 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn two_daemons_redeem_and_serve_a_slice_over_iroh() {
    let origin = Host::new(true, true);
    let peer = Host::new(false, false);
    let doc = origin.brain.join(Path::new("knowledge/shared/fact.md"));
    std::fs::create_dir_all(doc.parent().unwrap()).unwrap();
    std::fs::write(&doc, "# Shared fact\n\n- networkneedle crosses iroh\n").unwrap();
    origin.run_ok(&["scan"]);

    let origin_daemon = origin.daemon();
    let _peer_daemon = peer.daemon();
    let ticket = text(&origin.run_ok(&["invite", "shared", "--mode", "ro"]));
    assert!(ticket.starts_with("cfetch-network1-invite-3:"), "{ticket}");

    let joined = peer.run_ok(&["join", &ticket, "--json"]);
    let joined: serde_json::Value = serde_json::from_slice(&joined.stdout).unwrap();
    assert_eq!(joined["shared_tree"], false);
    assert_eq!(joined["slice"], "shared");

    let recalled = peer.run_ok(&["recall", "--slice", "shared", "networkneedle"]);
    let recalled = text(&recalled);
    assert!(recalled.contains("networkneedle"), "{recalled}");
    assert!(recalled.contains("served by iroh-origin"), "{recalled}");

    let diagnosis = peer.run_ok(&["doctor", "--json"]);
    let diagnosis: serde_json::Value = serde_json::from_slice(&diagnosis.stdout).unwrap();
    assert_eq!(diagnosis["topology"]["joined_origins"][0]["slice"], "shared");
    assert_eq!(
        diagnosis["topology"]["joined_origins"][0]["reachability"],
        "reachable",
        "doctor must prove the authorized serving path, not just list remembered state: {diagnosis}"
    );
    assert!(diagnosis["topology"]["joined_origins"][0]["generation"].is_number());

    let membership = std::fs::read_to_string(peer.state.join("memberships.json")).unwrap();
    let secret = grant_secret(&ticket);
    assert!(!membership.contains(&secret), "one-time invite secret was persisted");

    let grants = origin.run_ok(&["grants", "--json"]);
    let grants: serde_json::Value = serde_json::from_slice(&grants.stdout).unwrap();
    assert_eq!(grants["grants"][0]["state"], "redeemed");

    drop(origin_daemon);
    let diagnosis = peer.run_ok(&["doctor", "--json"]);
    let diagnosis: serde_json::Value = serde_json::from_slice(&diagnosis.stdout).unwrap();
    assert_eq!(
        diagnosis["topology"]["joined_origins"][0]["reachability"],
        "unreachable",
        "remembered membership must not be rendered as a live connection: {diagnosis}"
    );
}

#[test]
fn second_storage_group_fetches_vectors_without_an_embedding_call() {
    let embeddings = EmbeddingServer::start();
    let origin = Host::new_with_embeddings(true, true, Some(&embeddings.endpoint));
    let peer = Host::new(false, false);
    for host in [&origin, &peer] {
        let doc = host.brain.join(Path::new("knowledge/shared/fact.md"));
        std::fs::create_dir_all(doc.parent().unwrap()).unwrap();
        std::fs::write(&doc, "# Shared fact\n\n- artifactneedle is derived once\n").unwrap();
        host.run_ok(&["scan"]);
    }
    origin.run_ok(&["embed-index", "--batch", "64"]);
    let origin_calls = embeddings.calls.load(Ordering::SeqCst);
    assert!(origin_calls > 0, "origin must derive the initial vectors");

    let _origin_daemon = origin.daemon();
    let _peer_daemon = peer.daemon();
    let ticket = text(&origin.run_ok(&["invite", "shared", "--mode", "ro"]));
    peer.run_ok(&["join", &ticket, "--json"]);

    let synced = peer.run_ok(&["embed-index", "--batch", "64"]);
    let synced = text(&synced);
    assert!(
        synced.contains("authorized peers over iroh-blobs (no embedding call)"),
        "peer did not report its artifact route: {synced}"
    );
    assert!(synced.contains("0 embedded this run"), "peer re-derived work: {synced}");
    assert_eq!(
        embeddings.calls.load(Ordering::SeqCst),
        origin_calls,
        "the receiving host must fetch every matching vector and make zero embedding calls"
    );
}

fn grant_secret(ticket: &str) -> String {
    // The test only uses this to prove the secret is absent from peer state.
    // Decode the base64url body with the same tiny, dependency-free alphabet
    // as the product ticket format.
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let body = ticket.strip_prefix("cfetch-network1-invite-3:").unwrap();
    let values: Vec<u8> = body
        .bytes()
        .map(|c| B64.iter().position(|&x| x == c).unwrap() as u8)
        .collect();
    let mut raw = Vec::new();
    for chunk in values.chunks(4) {
        let mut n = 0u32;
        for (i, value) in chunk.iter().enumerate() {
            n |= (*value as u32) << (18 - 6 * i);
        }
        for i in 0..chunk.len() - 1 {
            raw.push(((n >> (16 - 8 * i)) & 0xff) as u8);
        }
    }
    serde_json::from_slice::<serde_json::Value>(&raw).unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_string()
}
