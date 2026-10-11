//! The `borg` binary's queen administration commands and `--token-file`.

use std::fs;
use std::path::Path;

use borg::drone::exec::{Exec, SystemExec};

/// Cargo supplies the binary path at compile time for `cargo test`; an
/// all-target check can compile this file without it, so fall back to the
/// binary beside the test's deps directory.
fn borg_bin() -> String {
    option_env!("CARGO_BIN_EXE_borg")
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let exe = std::env::current_exe().expect("test executable path");
            let dir = exe
                .parent()
                .and_then(Path::parent)
                .expect("target profile directory");
            dir.join("borg").to_string_lossy().into_owned()
        })
}

fn run(args: &[&str]) -> (bool, String) {
    let o = SystemExec.run(&borg_bin(), args).unwrap();
    (o.success, o.stdout)
}

fn write_config(dir: &Path) -> String {
    let path = dir.join("queen.json");
    let db = dir.join("queen.db");
    fs::write(
        &path,
        format!(
            r#"{{"github":{{"org":"example-org","app_id":"1"}},"repos":["example-org/app"],"db_path":"{}"}}"#,
            db.display()
        ),
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn enroll_prints_only_the_token_and_list_and_remove_manage_drones() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = write_config(dir.path());

    let (ok, token) = run(&["queen", "enroll", "--config", &cfg, "host-a"]);
    assert!(ok);
    assert!(
        token.starts_with("borg_")
            && token.len() == 5 + 64
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "stdout is exactly the token: {token:?}"
    );

    let (ok, list) = run(&["queen", "drone", "list", "--config", &cfg]);
    assert!(ok);
    assert!(list.contains("host-a") && list.contains("never"), "{list}");
    assert!(!list.contains(&token), "the token is never shown again");

    let (ok, removed) = run(&["queen", "drone", "remove", "--config", &cfg, "host-a"]);
    assert!(ok && removed.contains("removed drone host-a"), "{removed}");
    let (_, list) = run(&["queen", "drone", "list", "--config", &cfg]);
    assert!(!list.contains("host-a"), "{list}");
    let (ok, _) = run(&["queen", "drone", "remove", "--config", &cfg, "host-a"]);
    assert!(!ok, "removing an unknown drone fails");
}

#[test]
fn install_reads_the_token_from_a_file_and_never_needs_it_on_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    fs::write(&token, "borg_from_a_file\n").unwrap();
    let me = SystemExec.run("id", &["-un"]).unwrap().stdout;
    let root = dir.path().join("root");
    // Linux hosts must be running systemd; stand in for that under --root.
    fs::create_dir_all(root.join("run/systemd/system")).unwrap();
    let (ok, plan) = run(&[
        "drone",
        "install",
        "--queen",
        "https://queen.example.test",
        "--token-file",
        &token.to_string_lossy(),
        "--dry-run",
        "--no-job-user",
        "--run-as",
        &me,
        "--root",
        &root.to_string_lossy(),
    ]);
    assert!(ok, "{plan}");
    assert!(plan.contains("drone.token"), "{plan}");
    assert!(
        !plan.contains("borg_from_a_file"),
        "the token is never printed"
    );

    let (ok, _) = run(&[
        "drone",
        "install",
        "--queen",
        "https://queen.example.test",
        "--dry-run",
        "--no-job-user",
        "--run-as",
        &me,
    ]);
    assert!(!ok, "a token is required");
}
