//! The diagnostic command is useful before networking or inference has ever
//! run, and observing that blank state must not create a host identity.

use std::process::Command;

#[test]
fn doctor_json_is_read_only_and_labels_unmeasured_state() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let state = root.path().join("state");
    let brain = root.path().join("brain");
    for dir in [&home, &state, &brain] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let output = Command::new(env!("CARGO_BIN_EXE_cfetch"))
        .args(["doctor", "--json", "--no-network"])
        .env("HOME", &home)
        .env("CFETCH_STATE_DIR", &state)
        .env("CFETCH_BRAIN", &brain)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["daemon"]["state"], "stopped");
    assert_eq!(report["inference"]["utilization"]["state"], "not_reported");
    assert!(
        report["hardware"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()),
        "the CPU floor must always be visible: {report}"
    );
    assert!(
        !state.join("endpoint.key").exists(),
        "reading diagnostics must not create a network identity"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_cfetch"))
        .args(["doctor", "--no-network"])
        .env("HOME", &home)
        .env("CFETCH_STATE_DIR", &state)
        .env("CFETCH_BRAIN", &brain)
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Detected hardware"), "{text}");
    assert!(text.contains("not supported by this build"), "{text}");
    assert!(text.contains("live utilization: not reported"), "{text}");
    assert!(text.contains("no network identity yet"), "{text}");
}
