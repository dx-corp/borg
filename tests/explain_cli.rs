//! Exercise the public CLI's local and authenticated HTTP history reads.
use crate::support;

use std::path::Path;
use std::process::Output;

use borg::allocator::{AllocatorInput, Demand, DroneState, HostCaps, allocate};
use borg::bid::ScoreWeights;
use borg::classes::{Class, Classes, Scope};
use borg::explain::{Explanation, PlacementKind, PlacementOutcome, PlacementRecord};
use borg::store::Store;
use serde_json::json;
use support::Harness;

fn borg_bin() -> String {
    option_env!("CARGO_BIN_EXE_borg")
        .map(str::to_owned)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .and_then(Path::parent)
                .unwrap()
                .join("borg")
                .to_string_lossy()
                .into_owned()
        })
}

async fn run(args: &[&str], home: &Path) -> Output {
    #[allow(
        clippy::disallowed_methods,
        reason = "CLI boundary test: execute the compiled borg binary with isolated credential sources"
    )]
    tokio::process::Command::new(borg_bin())
        .args(args)
        .env("HOME", home)
        .env_remove("BORG_CLIENT_TOKEN")
        .env_remove("BORG_CLIENT_TOKEN_FILE")
        .output()
        .await
        .unwrap()
}

async fn record(store: &Store, kind: PlacementKind, key: &str, client: Option<&str>) {
    let input = AllocatorInput {
        classes: Classes::default(),
        demand: vec![Demand {
            class: Class::Light,
            scope: Scope::Main,
            count: 1,
        }],
        drones: vec![DroneState::from_capacity(
            "host-a",
            &support::capacity(),
            &HostCaps::default(),
        )],
        leases: vec![],
        warm_floors: vec![],
        queue_ages: vec![],
        starvation_after_secs: None,
        weights: ScoreWeights::default(),
        admission: None,
    };
    store
        .record_placement(PlacementRecord {
            at: 123,
            kind,
            key,
            client,
            outcome: PlacementOutcome::Granted,
            decision: &allocate(&input).decisions[0],
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn local_config_resolves_aliases_and_renders_real_typed_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queen.db");
    let cfg = dir.path().join("queen.json");
    std::fs::write(
        &cfg,
        json!({"github":{"org":"example-org","app_id":"1"},"repos":["example-org/app"],
        "db_path":path,"classes":{"aliases":{"build-light":"light"}}})
        .to_string(),
    )
    .unwrap();
    let store = Store::open(&path).await.unwrap();
    record(&store, PlacementKind::Runner, "borg-ci-light", None).await;
    let output = run(
        &[
            "explain",
            "build-light",
            "--config",
            cfg.to_str().unwrap(),
            "--json",
        ],
        dir.path(),
    )
    .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Explanation = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report.key, "build-light");
    assert_eq!(report.history.len(), 1);
    assert_eq!(report.history[0].key, "borg-ci-light");
    assert_eq!(report.history[0].decision.winner.as_deref(), Some("host-a"));
    let text = run(
        &["explain", "build-light", "--config", cfg.to_str().unwrap()],
        dir.path(),
    )
    .await;
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("selected host-a"), "{text}");
    assert!(text.contains("bid 0, profile 0"), "{text}");
    let unknown = run(
        &["explain", "unknown-unit", "--config", cfg.to_str().unwrap()],
        dir.path(),
    )
    .await;
    assert!(unknown.status.success());
    assert!(
        String::from_utf8(unknown.stdout)
            .unwrap()
            .contains("no retained placement decision")
    );
}

#[tokio::test]
async fn remote_unit_cli_uses_token_file_and_does_not_disclose_another_clients_history() {
    let h = Harness::start(json!({"units":{"enabled":true}})).await;
    for (name, token) in [
        ("alice", "alice-private-token"),
        ("bob", "bob-private-token"),
    ] {
        h.queen
            .store()
            .enroll_client(name, &borg::enroll::hash_token(token), 0)
            .await
            .unwrap();
    }
    let response = h.http.post(format!("{}/v1/units", h.url)).bearer_auth("alice-private-token")
        .json(&json!({"repo":"example-org/app","tree_sha":"0123456789abcdef0123456789abcdef01234567", "argv":["do-private-work"],"platform":"linux-x86_64","class":"light"})).send().await.unwrap();
    assert_eq!(response.status(), 201);
    let submitted: serde_json::Value = response.json().await.unwrap();
    let id = submitted["unit_id"].as_str().unwrap();
    record(h.queen.store(), PlacementKind::Unit, id, Some("alice")).await;
    record(
        h.queen.store(),
        PlacementKind::Runner,
        "borg-ci-light",
        None,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("client-token");
    std::fs::write(&token, "alice-private-token\n").unwrap();
    let args = [
        "explain",
        id,
        "--queen",
        &h.url,
        "--token-file",
        token.to_str().unwrap(),
        "--json",
    ];
    let alice = run(&args, dir.path()).await;
    assert!(
        alice.status.success(),
        "{}",
        String::from_utf8_lossy(&alice.stderr)
    );
    let report: Explanation = serde_json::from_slice(&alice.stdout).unwrap();
    assert_eq!(report.history.len(), 1);
    assert_eq!(report.history[0].key, id);
    assert!(!String::from_utf8_lossy(&alice.stdout).contains("do-private-work"));
    assert!(!String::from_utf8_lossy(&alice.stderr).contains("alice-private-token"));
    std::fs::write(&token, "bob-private-token\n").unwrap();
    let bob = run(&args, dir.path()).await;
    assert!(bob.status.success());
    let report: Explanation = serde_json::from_slice(&bob.stdout).unwrap();
    assert!(report.history.is_empty());
    let anonymous = run(&["explain", id, "--queen", &h.url, "--json"], dir.path()).await;
    assert!(!anonymous.status.success());
    assert!(anonymous.stdout.is_empty());
    let public = run(
        &["explain", "borg-ci-light", "--queen", &h.url, "--json"],
        dir.path(),
    )
    .await;
    assert!(public.status.success());
    let report: Explanation = serde_json::from_slice(&public.stdout).unwrap();
    assert_eq!(report.history.len(), 1);
    assert_eq!(report.history[0].kind, PlacementKind::Runner);
}

#[tokio::test]
async fn cli_rejects_missing_or_conflicting_sources() {
    let dir = tempfile::tempdir().unwrap();
    let missing = run(&["explain", "borg-ci-light"], dir.path()).await;
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    let conflict = run(
        &[
            "explain",
            "borg-ci-light",
            "--queen",
            "http://127.0.0.1:1",
            "--config",
            "queen.json",
        ],
        dir.path(),
    )
    .await;
    assert!(!conflict.status.success());
    assert!(conflict.stdout.is_empty());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("cannot be used with"));
}
