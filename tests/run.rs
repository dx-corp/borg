//! `borg run` against a fake queen and a real temporary git repository.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use borg::run::{
    RunOptions, TreeSource, parse_toolchain_channel, repo_from_remote, resolve_tree, run,
    tree_rust_toolchain,
};
use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Receipt,
    BadReusedDigest,
    Run,
    Failed,
    Queued,
    Joined,
    LostLog,
    JoinedLostLog,
    JoinedRun,
    MultiChunk,
    RunningForever,
    Unauthorized,
    BadTerminalReceipt,
    BadTerminalDigest,
    Refuse,
    /// Runs like `Run` but rejects the bundle upload.
    RefuseBundle,
}

struct Fake {
    mode: Mode,
    submitted: Mutex<Vec<Value>>,
    cancelled: Mutex<u32>,
    bundles: Mutex<Vec<Vec<u8>>>,
    log_reads: Mutex<u32>,
}

type Shared = Arc<Fake>;

fn receipt_for(command: &Value) -> borg::units::Receipt {
    let mut receipt = borg::units::Receipt {
        unit_id: "u-1".into(),
        repo: command["repo"].as_str().unwrap().into(),
        tree_sha: command["tree_sha"].as_str().unwrap().into(),
        argv: serde_json::from_value(command["argv"].clone()).unwrap(),
        platform: command["platform"].as_str().unwrap().into(),
        toolchain: command["toolchain"].as_str().unwrap().into(),
        drone: "d9".into(),
        exit_code: 0,
        finished_at: 123,
        duration_secs: 2,
        digest: String::new(),
    };
    receipt.digest = receipt.compute_digest();
    receipt
}

async fn submit(State(f): State<Shared>, Json(b): Json<Value>) -> Response {
    f.submitted.lock().unwrap().push(b.clone());
    match f.mode {
        Mode::Joined | Mode::JoinedLostLog | Mode::JoinedRun => {
            Json(json!({"outcome": "joined", "unit_id": "u-1", "state": "queued"})).into_response()
        }
        Mode::Refuse => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        Mode::Unauthorized => StatusCode::UNAUTHORIZED.into_response(),
        Mode::Receipt | Mode::BadReusedDigest => {
            let mut receipt = receipt_for(&b);
            if f.mode == Mode::BadReusedDigest {
                receipt.digest = "tampered".into();
            }
            Json(json!({"outcome": "receipt", "unit_id": "u-1", "state": "done", "receipt": receipt})).into_response()
        }
        _ => (
            StatusCode::CREATED,
            Json(json!({"outcome": "new", "unit_id": "u-1", "state": "queued"})),
        )
            .into_response(),
    }
}

async fn show(State(f): State<Shared>, AxPath(_id): AxPath<String>) -> Json<Value> {
    let mut unit = f.submitted.lock().unwrap().last().unwrap().clone();
    unit["id"] = json!("u-1");
    unit["drone"] = json!("d9");
    unit["timeout_secs"] = json!(if f.mode == Mode::RunningForever {
        1
    } else {
        30
    });
    unit["state"] = json!(match f.mode {
        Mode::Run
        | Mode::RefuseBundle
        | Mode::BadTerminalReceipt
        | Mode::BadTerminalDigest
        | Mode::JoinedRun
        | Mode::MultiChunk => "done",
        Mode::RunningForever | Mode::LostLog | Mode::JoinedLostLog => "running",
        Mode::Failed => "failed",
        _ => "queued",
    });
    unit["exit_code"] = json!(if matches!(
        f.mode,
        Mode::BadTerminalReceipt | Mode::BadTerminalDigest
    ) {
        0
    } else {
        3
    });
    if f.mode == Mode::BadTerminalReceipt {
        unit["receipt"] = json!({"unit_id": "u-1", "repo": "other/repo",
            "tree_sha": unit["tree_sha"], "argv": unit["argv"],
            "platform": unit["platform"], "toolchain": unit["toolchain"],
            "drone": "d9", "exit_code": 0, "finished_at": 123,
            "duration_secs": 1, "digest": "tampered"});
    }
    if f.mode == Mode::BadTerminalDigest {
        let mut receipt = receipt_for(&unit);
        receipt.digest = "tampered".into();
        unit["receipt"] = serde_json::to_value(receipt).unwrap();
    }
    Json(unit)
}

async fn log(State(f): State<Shared>, AxPath(_id): AxPath<String>) -> Response {
    if matches!(f.mode, Mode::LostLog | Mode::JoinedLostLog) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if f.mode == Mode::MultiChunk {
        let mut reads = f.log_reads.lock().unwrap();
        *reads += 1;
        let last = *reads > 1;
        let mut response = if last {
            "second chunk\n"
        } else {
            "first chunk\n"
        }
        .into_response();
        let headers = response.headers_mut();
        headers.insert("x-borg-state", "done".parse().unwrap());
        headers.insert(
            "x-borg-next",
            if last { "25" } else { "12" }.parse().unwrap(),
        );
        if last {
            headers.insert("x-borg-exit", "3".parse().unwrap());
        }
        return response;
    }
    let mut r = match f.mode {
        Mode::Run
        | Mode::RefuseBundle
        | Mode::BadTerminalReceipt
        | Mode::BadTerminalDigest
        | Mode::JoinedRun => "hello from the fleet\n".into_response(),
        _ => {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            "".into_response()
        }
    };
    let h = r.headers_mut();
    let (state, next) = match f.mode {
        Mode::Run
        | Mode::RefuseBundle
        | Mode::BadTerminalReceipt
        | Mode::BadTerminalDigest
        | Mode::JoinedRun => ("done", "21"),
        Mode::Failed => ("failed", "0"),
        Mode::RunningForever => ("running", "0"),
        _ => ("queued", "0"),
    };
    h.insert("x-borg-state", state.parse().unwrap());
    h.insert("x-borg-next", next.parse().unwrap());
    if matches!(
        f.mode,
        Mode::Run
            | Mode::RefuseBundle
            | Mode::BadTerminalReceipt
            | Mode::BadTerminalDigest
            | Mode::JoinedRun
    ) {
        h.insert(
            "x-borg-exit",
            if matches!(f.mode, Mode::BadTerminalReceipt | Mode::BadTerminalDigest) {
                "0"
            } else {
                "3"
            }
            .parse()
            .unwrap(),
        );
    }
    if f.mode == Mode::Failed {
        h.insert("x-borg-failure", "timed out".parse().unwrap());
    }
    r
}

async fn cancel(State(f): State<Shared>, AxPath(_id): AxPath<String>) -> StatusCode {
    *f.cancelled.lock().unwrap() += 1;
    StatusCode::NO_CONTENT
}

async fn bundle(
    State(f): State<Shared>,
    AxPath(_id): AxPath<String>,
    body: axum::body::Bytes,
) -> StatusCode {
    if f.mode == Mode::RefuseBundle {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    f.bundles.lock().unwrap().push(body.to_vec());
    StatusCode::NO_CONTENT
}

async fn fake_queen(mode: Mode) -> (String, Shared) {
    let f = Arc::new(Fake {
        mode,
        submitted: Mutex::new(Vec::new()),
        cancelled: Mutex::new(0),
        bundles: Mutex::new(Vec::new()),
        log_reads: Mutex::new(0),
    });
    let app = Router::new()
        .route("/v1/units", post(submit))
        .route("/v1/units/{id}", get(show))
        .route("/v1/units/{id}/log", get(log))
        .route("/v1/units/{id}/bundle", axum::routing::put(bundle))
        .route("/v1/units/{id}/cancel", post(cancel))
        .with_state(Arc::clone(&f));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await });
    (url, f)
}

fn git(dir: &Path, args: &[&str]) -> String {
    #[allow(
        clippy::disallowed_methods,
        reason = "test fixture: scratch git repositories built with the real git binary"
    )]
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn git_succeeds(dir: &Path, args: &[&str]) -> bool {
    #[allow(
        clippy::disallowed_methods,
        reason = "test fixture: asks the real git binary whether an object exists"
    )]
    let status = Command::new("git").args(args).current_dir(dir).status();
    status.is_ok_and(|s| s.success())
}

/// A repository with one pushed commit and its bare origin.
struct Repo {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    origin: PathBuf,
}

fn repo() -> Repo {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let dir = root.join("work");
    let origin = root.join("origin.git");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "first"]);
    git(&dir, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&dir, &["push", "-q", "origin", "main"]);
    Repo {
        _tmp: tmp,
        dir,
        origin,
    }
}

fn opts(r: &Repo, queen: Option<String>, marker: &str) -> RunOptions {
    RunOptions {
        token: queen
            .as_ref()
            .map(|_| borg::secret::Secret::new("borg_client")),
        queen,
        dir: r.dir.clone(),
        tree: TreeSource::Head,
        repo: Some("example-org/app".into()),
        platform: "linux-x86_64".into(),
        class: "medium".into(),
        toolchain: "tc".into(),
        timeout_secs: None,
        force: false,
        scope: "pr".into(),
        wait_secs: 1,
        publish: false,
        remote_only: false,
        result_file: None,
        argv: vec!["sh".into(), "-c".into(), format!("touch {marker}; exit 7")],
    }
}

fn marker(r: &Repo) -> String {
    r.dir.join("ran-locally").display().to_string()
}

#[test]
fn trees_and_repository_names_are_resolved_from_git() {
    let r = repo();
    let head = resolve_tree(&r.dir, &TreeSource::Head).unwrap();
    assert_eq!(head, git(&r.dir, &["rev-parse", "HEAD^{tree}"]));
    assert_eq!(resolve_tree(&r.dir, &TreeSource::Index).unwrap(), head);
    // Staging a change moves the index tree, not HEAD's.
    std::fs::write(r.dir.join("a.txt"), "two\n").unwrap();
    git(&r.dir, &["add", "a.txt"]);
    let staged = resolve_tree(&r.dir, &TreeSource::Index).unwrap();
    assert_ne!(staged, head);
    assert_eq!(resolve_tree(&r.dir, &TreeSource::Head).unwrap(), head);
    // A commit id resolves to its tree.
    let commit = git(&r.dir, &["rev-parse", "HEAD"]);
    assert_eq!(
        resolve_tree(&r.dir, &TreeSource::Explicit(commit)).unwrap(),
        head
    );
    assert!(resolve_tree(&r.dir, &TreeSource::Explicit("nope".into())).is_err());

    for (url, want) in [
        ("git@github.com:dx-corp/mono.git", Some("dx-corp/mono")),
        ("https://github.com/dx-corp/mono", Some("dx-corp/mono")),
        ("https://github.com/dx-corp/mono.git/", Some("dx-corp/mono")),
        ("mono", None),
    ] {
        assert_eq!(repo_from_remote(url).as_deref(), want, "{url}");
    }
}

#[tokio::test]
async fn without_a_queen_the_command_just_runs_here() {
    let r = repo();
    let code = run(&opts(&r, None, &marker(&r)), &mut Vec::new()).await;
    assert_eq!(code, 7);
    assert!(Path::new(&marker(&r)).exists());
}

#[tokio::test]
async fn a_receipt_means_nothing_runs_and_the_exit_code_is_zero() {
    let r = repo();
    let (url, fake) = fake_queen(Mode::Receipt).await;
    let code = run(&opts(&r, Some(url), &marker(&r)), &mut Vec::new()).await;
    assert_eq!(code, 0);
    assert!(!Path::new(&marker(&r)).exists());
    let sent = fake.submitted.lock().unwrap()[0].clone();
    assert_eq!(
        sent["tree_sha"],
        resolve_tree(&r.dir, &TreeSource::Head).unwrap()
    );
    assert_eq!(sent["repo"], "example-org/app");
    assert_eq!(sent["argv"][0], "sh");
    assert_eq!(sent["platform"], "linux-x86_64");
}

#[tokio::test]
async fn a_remote_run_streams_the_log_and_returns_the_units_exit_code() {
    let r = repo();
    let (url, _fake) = fake_queen(Mode::Run).await;
    let mut out = Vec::new();
    let code = run(&opts(&r, Some(url), &marker(&r)), &mut out).await;
    assert_eq!(code, 3, "the unit's code, not the local command's");
    assert_eq!(String::from_utf8(out).unwrap(), "hello from the fleet\n");
    assert!(!Path::new(&marker(&r)).exists());
}

#[tokio::test]
async fn the_index_tree_is_what_a_hook_submits_and_travels_as_a_small_bundle() {
    let r = repo();
    std::fs::write(r.dir.join("a.txt"), "staged\n").unwrap();
    git(&r.dir, &["add", "a.txt"]);
    let tree = git(&r.dir, &["write-tree"]);
    let (url, fake) = fake_queen(Mode::Run).await;
    let mut o = opts(&r, Some(url), &marker(&r));
    o.tree = TreeSource::Index;
    // The staged tree is not on any remote: without --publish this runs locally.
    assert_eq!(run(&o, &mut Vec::new()).await, 7);
    assert!(fake.submitted.lock().unwrap().is_empty());

    // With --publish it is submitted with a bundle, and nothing is pushed.
    o.publish = true;
    assert_eq!(run(&o, &mut Vec::new()).await, 3);
    let sent = fake.submitted.lock().unwrap()[0].clone();
    assert_eq!(sent["tree_sha"], tree.as_str());
    assert_eq!(sent["bundle"], true);
    let bundle = fake.bundles.lock().unwrap()[0].clone();
    assert!(bundle.starts_with(b"# v2 git bundle"));
    assert!(
        git(&r.origin, &["for-each-ref"])
            .lines()
            .all(|l| !l.contains("refs/borg"))
    );
    assert!(
        git(&r.dir, &["for-each-ref"])
            .lines()
            .all(|l| !l.contains("refs/borg"))
    );

    // What a drone does: a clone of the remote (its mirror) unbundles it and
    // then has the tree, though it never had the staged content.
    let mirror = r.dir.parent().unwrap().join("mirror");
    git(
        &r.dir,
        &[
            "clone",
            "-q",
            "--bare",
            r.origin.to_str().unwrap(),
            mirror.to_str().unwrap(),
        ],
    );
    assert!(!git_succeeds(&mirror, &["cat-file", "-e", &tree]));
    let file = mirror.join("unit.bundle");
    std::fs::write(&file, &bundle).unwrap();
    git(&mirror, &["bundle", "unbundle", file.to_str().unwrap()]);
    assert_eq!(git(&mirror, &["cat-file", "-t", &tree]), "tree");
    assert_eq!(git(&mirror, &["show", &format!("{tree}:a.txt")]), "staged");
}

#[tokio::test]
async fn a_pushed_tree_sends_no_bundle_and_a_refused_bundle_falls_back() {
    let r = repo();
    let (url, fake) = fake_queen(Mode::Run).await;
    let mut o = opts(&r, Some(url), &marker(&r));
    o.publish = true;
    assert_eq!(run(&o, &mut Vec::new()).await, 3);
    assert_eq!(fake.submitted.lock().unwrap()[0]["bundle"], false);
    assert!(fake.bundles.lock().unwrap().is_empty());

    // The queen refuses the bundle (too large): the unit is cancelled and the
    // command runs here.
    std::fs::write(r.dir.join("a.txt"), "staged\n").unwrap();
    git(&r.dir, &["add", "a.txt"]);
    let (url, fake) = fake_queen(Mode::RefuseBundle).await;
    let mut o = opts(&r, Some(url), &marker(&r));
    o.tree = TreeSource::Index;
    o.publish = true;
    assert_eq!(run(&o, &mut Vec::new()).await, 7);
    assert_eq!(*fake.cancelled.lock().unwrap(), 1);
    assert!(Path::new(&marker(&r)).exists());
}

#[tokio::test]
async fn an_unreachable_or_refusing_queen_falls_back_to_running_here() {
    let r = repo();
    // Nothing listens on this port.
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    assert_eq!(
        run(&opts(&r, Some(dead), &marker(&r)), &mut Vec::new()).await,
        7
    );
    assert!(Path::new(&marker(&r)).exists());
    std::fs::remove_file(marker(&r)).unwrap();

    let (url, _) = fake_queen(Mode::Refuse).await;
    assert_eq!(
        run(&opts(&r, Some(url), &marker(&r)), &mut Vec::new()).await,
        7
    );
    assert!(Path::new(&marker(&r)).exists());
}

#[tokio::test]
async fn no_drone_within_the_wait_cancels_the_unit_and_runs_here() {
    let r = repo();
    let (url, fake) = fake_queen(Mode::Queued).await;
    let started = std::time::Instant::now();
    assert_eq!(
        run(&opts(&r, Some(url), &marker(&r)), &mut Vec::new()).await,
        7
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(*fake.cancelled.lock().unwrap(), 1);
    assert!(Path::new(&marker(&r)).exists());
}

#[tokio::test]
async fn a_unit_that_failed_for_infrastructure_reasons_is_run_here_instead() {
    let r = repo();
    let (url, _) = fake_queen(Mode::Failed).await;
    assert_eq!(
        run(&opts(&r, Some(url), &marker(&r)), &mut Vec::new()).await,
        7
    );
    assert!(Path::new(&marker(&r)).exists());
}

#[test]
fn the_rust_toolchain_is_read_from_the_tree() {
    for (text, want) in [
        (
            "[toolchain]\nchannel = \"1.95.0\"\ncomponents = [\"clippy\"]\n",
            Some("1.95.0"),
        ),
        (
            "# pinned\n[toolchain]\nchannel=\"nightly-2026-01-01\" # nightly\n",
            Some("nightly-2026-01-01"),
        ),
        ("stable\n", Some("stable")),
        ("[toolchain]\ncomponents = []\n", None),
        ("[toolchain]\nchannel = \"bad toolchain!\"\n", None),
        ("", None),
    ] {
        assert_eq!(parse_toolchain_channel(text).as_deref(), want, "{text:?}");
    }
    let r = repo();
    assert_eq!(tree_rust_toolchain(&r.dir, "HEAD^{tree}"), None);
    std::fs::create_dir_all(r.dir.join("rust")).unwrap();
    std::fs::write(
        r.dir.join("rust/rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.95.0\"\n",
    )
    .unwrap();
    git(&r.dir, &["add", "."]);
    git(&r.dir, &["commit", "-q", "-m", "toolchain"]);
    assert_eq!(
        tree_rust_toolchain(&r.dir, "HEAD^{tree}").as_deref(),
        Some("1.95.0")
    );
    // The tree asked about decides, not the working directory.
    assert_eq!(tree_rust_toolchain(&r.dir, "HEAD~1^{tree}"), None);
}

#[tokio::test]
async fn the_submission_carries_the_trees_toolchain() {
    let r = repo();
    std::fs::write(
        r.dir.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.95.0\"\n",
    )
    .unwrap();
    git(&r.dir, &["add", "."]);
    git(&r.dir, &["commit", "-q", "-m", "toolchain"]);
    git(&r.dir, &["push", "-q", "origin", "main"]);
    let (url, fake) = fake_queen(Mode::Receipt).await;
    assert_eq!(
        run(&opts(&r, Some(url), &marker(&r)), &mut Vec::new()).await,
        0
    );
    assert_eq!(
        fake.submitted.lock().unwrap()[0]["rust_toolchain"],
        "1.95.0"
    );
}

#[tokio::test]
async fn remote_only_never_runs_the_local_command_on_infrastructure_failures() {
    for mode in [
        Mode::Refuse,
        Mode::Unauthorized,
        Mode::Failed,
        Mode::Queued,
        Mode::LostLog,
    ] {
        let r = repo();
        let (url, _) = fake_queen(mode).await;
        let mut o = opts(&r, Some(url), &marker(&r));
        o.remote_only = true;
        o.wait_secs = 0;
        o.result_file = Some(r.dir.join("result.json"));
        assert_eq!(run(&o, &mut Vec::new()).await, 125);
        assert!(!Path::new(&marker(&r)).exists());
        let result: Value =
            serde_json::from_slice(&std::fs::read(o.result_file.unwrap()).unwrap()).unwrap();
        assert_eq!(result["exit_code"], 125);
        assert!(result["infrastructure_error"].is_string());
    }
}

#[tokio::test]
async fn remote_only_requires_configuration_a_resolvable_published_tree_and_a_reachable_queen() {
    let r = repo();
    let mut o = opts(&r, None, &marker(&r));
    o.remote_only = true;
    assert_eq!(run(&o, &mut Vec::new()).await, 125);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    o.queen = Some(dead);
    o.token = Some(borg::secret::Secret::new("token"));
    assert_eq!(run(&o, &mut Vec::new()).await, 125);
    o.tree = TreeSource::Explicit("missing-tree".into());
    assert_eq!(run(&o, &mut Vec::new()).await, 125);
    std::fs::write(r.dir.join("a.txt"), "staged\n").unwrap();
    git(&r.dir, &["add", "a.txt"]);
    o.tree = TreeSource::Index;
    assert_eq!(run(&o, &mut Vec::new()).await, 125);
    let (url, fake) = fake_queen(Mode::RefuseBundle).await;
    o.queen = Some(url);
    o.publish = true;
    assert_eq!(run(&o, &mut Vec::new()).await, 125);
    assert_eq!(*fake.cancelled.lock().unwrap(), 1);
    assert!(!Path::new(&marker(&r)).exists());
}

#[tokio::test]
async fn leaving_a_joined_unit_does_not_cancel_another_subscribers_work() {
    for remote_only in [false, true] {
        let r = repo();
        let (url, fake) = fake_queen(Mode::Joined).await;
        let mut o = opts(&r, Some(url), &marker(&r));
        o.remote_only = remote_only;
        o.wait_secs = 0;
        assert_eq!(
            run(&o, &mut Vec::new()).await,
            if remote_only { 125 } else { 7 }
        );
        assert_eq!(*fake.cancelled.lock().unwrap(), 0);
        assert_eq!(Path::new(&marker(&r)).exists(), !remote_only);
    }
}

#[tokio::test]
async fn result_files_identify_remote_execution_and_reused_receipts_without_mixing_stdout() {
    for mode in [Mode::Run, Mode::Receipt] {
        let r = repo();
        let (url, _) = fake_queen(mode).await;
        let mut o = opts(&r, Some(url), &marker(&r));
        o.remote_only = true;
        o.result_file = Some(r.dir.join("result.json"));
        let mut out = Vec::new();
        assert_eq!(
            run(&o, &mut out).await,
            if mode == Mode::Receipt { 0 } else { 3 }
        );
        let result: Value =
            serde_json::from_slice(&std::fs::read(o.result_file.unwrap()).unwrap()).unwrap();
        assert_eq!(result["schema_version"], 1);
        assert_eq!(result["execution"], "remote");
        assert_eq!(result["host"], "d9");
        assert_eq!(result["unit_id"], "u-1");
        assert_eq!(
            result["requested_tree_sha"],
            resolve_tree(&r.dir, &TreeSource::Head).unwrap()
        );
        assert_eq!(result["argv"], json!(o.argv));
        assert_eq!(
            result["provenance"],
            if mode == Mode::Receipt {
                "reused"
            } else {
                "new"
            }
        );
        if mode == Mode::Receipt {
            assert_eq!(out, b"");
            assert_eq!(result["receipt"]["drone"], "d9");
        } else {
            assert_eq!(out, b"hello from the fleet\n");
        }
        assert!(!Path::new(&marker(&r)).exists());
    }
}

#[tokio::test]
async fn a_local_result_does_not_claim_execution_against_a_git_tree() {
    let r = repo();
    let mut o = opts(&r, None, &marker(&r));
    o.result_file = Some(r.dir.join("result.json"));
    assert_eq!(run(&o, &mut Vec::new()).await, 7);
    let result: Value =
        serde_json::from_slice(&std::fs::read(o.result_file.unwrap()).unwrap()).unwrap();
    assert_eq!(result["execution"], "local");
    assert_eq!(result["provenance"], "fallback");
    assert!(result["host"].is_null());
    assert!(result["receipt"].is_null());
}

#[test]
fn the_cli_remote_only_flag_prevents_local_execution_and_emits_a_result() {
    let r = repo();
    let result_path = r.dir.join("result.json");
    #[allow(
        clippy::disallowed_methods,
        reason = "test invokes the Borg CLI with a local sentinel"
    )]
    let output = Command::new(
        option_env!("CARGO_BIN_EXE_borg")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CARGO_BIN_EXE_borg").map(PathBuf::from))
            .expect("Cargo binary path available at runtime"),
    )
    .current_dir(&r.dir)
    .env_remove("BORG_QUEEN")
    .env_remove("BORG_RUN_REMOTE_ONLY")
    .args(["run", "--remote-only", "--result-file"])
    .arg(&result_path)
    .args(["--", "sh", "-c", "touch ran-locally"])
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(125));
    assert!(output.stdout.is_empty());
    assert!(!Path::new(&marker(&r)).exists());
    let result: Value = serde_json::from_slice(&std::fs::read(result_path).unwrap()).unwrap();
    assert_eq!(result["execution"], "none");
    assert_eq!(result["exit_code"], 125);
}

#[tokio::test]
async fn remote_only_enforces_the_admitted_command_deadline() {
    let r = repo();
    let (url, fake) = fake_queen(Mode::RunningForever).await;
    let mut o = opts(&r, Some(url), &marker(&r));
    o.remote_only = true;
    let started = std::time::Instant::now();
    assert_eq!(run(&o, &mut Vec::new()).await, 125);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(*fake.cancelled.lock().unwrap(), 1);
    assert!(!Path::new(&marker(&r)).exists());
}

#[tokio::test]
async fn broken_output_consumers_never_rerun_remote_commands_locally() {
    struct BrokenOutput {
        fail_flush: bool,
    }
    impl std::io::Write for BrokenOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.fail_flush {
                Ok(bytes.len())
            } else {
                Err(std::io::Error::other("closed log consumer"))
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_flush {
                Err(std::io::Error::other("cannot flush log consumer"))
            } else {
                Ok(())
            }
        }
    }
    for remote_only in [false, true] {
        for fail_flush in [false, true] {
            for mode in [Mode::Run, Mode::JoinedRun, Mode::MultiChunk] {
                let r = repo();
                let (url, fake) = fake_queen(mode).await;
                let mut o = opts(&r, Some(url), &marker(&r));
                o.remote_only = remote_only;
                o.result_file = Some(r.dir.join("result.json"));
                assert_eq!(
                    run(&o, &mut BrokenOutput { fail_flush }).await,
                    if remote_only { 125 } else { 3 }
                );
                assert_eq!(
                    *fake.cancelled.lock().unwrap(),
                    u32::from(remote_only && mode != Mode::JoinedRun)
                );
                assert!(!Path::new(&marker(&r)).exists());
                if mode == Mode::MultiChunk && !remote_only {
                    assert_eq!(*fake.log_reads.lock().unwrap(), 2);
                }
                let result: Value =
                    serde_json::from_slice(&std::fs::read(o.result_file.unwrap()).unwrap())
                        .unwrap();
                assert_eq!(result["execution"], "remote");
                assert!(
                    result["infrastructure_error"]
                        .as_str()
                        .unwrap()
                        .contains("remote log")
                );
            }
        }
    }
}

#[tokio::test]
async fn an_unwritable_result_destination_prevents_execution() {
    let r = repo();
    let mut o = opts(&r, None, &marker(&r));
    o.result_file = Some(r.dir.join("missing-parent/result.json"));
    assert_eq!(run(&o, &mut Vec::new()).await, 125);
    assert!(!Path::new(&marker(&r)).exists());
}

#[tokio::test]
async fn terminal_metadata_never_publishes_a_tampered_receipt_or_reexecutes_the_command() {
    let r = repo();
    let (url, _) = fake_queen(Mode::BadTerminalReceipt).await;
    let mut o = opts(&r, Some(url), &marker(&r));
    // Default execution can finish; optional metadata validation must not cause
    // a second local execution after the remote command has completed.
    o.result_file = Some(r.dir.join("result.json"));
    assert_eq!(run(&o, &mut Vec::new()).await, 0);
    let result: Value =
        serde_json::from_slice(&std::fs::read(o.result_file.unwrap()).unwrap()).unwrap();
    assert!(result["receipt"].is_null());
    assert!(result["host"].is_null());
    assert!(
        result["infrastructure_error"]
            .as_str()
            .unwrap()
            .contains("receipt")
    );
    assert!(!Path::new(&marker(&r)).exists());
}

#[tokio::test]
async fn completed_units_drain_all_log_chunks_before_returning_the_exit_code() {
    for remote_only in [false, true] {
        let r = repo();
        let (url, fake) = fake_queen(Mode::MultiChunk).await;
        let mut o = opts(&r, Some(url), &marker(&r));
        o.remote_only = remote_only;
        let mut out = Vec::new();
        assert_eq!(run(&o, &mut out).await, 3);
        assert_eq!(out, b"first chunk\nsecond chunk\n");
        assert_eq!(*fake.log_reads.lock().unwrap(), 2);
        assert_eq!(*fake.cancelled.lock().unwrap(), 0);
        assert!(!Path::new(&marker(&r)).exists());
    }
}

#[tokio::test]
async fn losing_remote_logs_cancels_only_a_newly_created_unit() {
    for mode in [Mode::LostLog, Mode::JoinedLostLog] {
        let r = repo();
        let (url, fake) = fake_queen(mode).await;
        let mut o = opts(&r, Some(url), &marker(&r));
        o.remote_only = true;
        o.wait_secs = 0;
        assert_eq!(run(&o, &mut Vec::new()).await, 125);
        assert_eq!(
            *fake.cancelled.lock().unwrap(),
            u32::from(mode == Mode::LostLog)
        );
        assert!(!Path::new(&marker(&r)).exists());
    }
}

#[tokio::test]
async fn a_matching_terminal_receipt_with_only_its_digest_corrupted_is_rejected() {
    let r = repo();
    let (url, _) = fake_queen(Mode::BadTerminalDigest).await;
    let mut o = opts(&r, Some(url), &marker(&r));
    o.result_file = Some(r.dir.join("result.json"));
    assert_eq!(run(&o, &mut Vec::new()).await, 0);
    let result: Value =
        serde_json::from_slice(&std::fs::read(o.result_file.unwrap()).unwrap()).unwrap();
    assert_eq!(result["execution"], "remote");
    assert_eq!(result["exit_code"], 0);
    assert!(result["receipt"].is_null());
    assert!(result["host"].is_null());
    assert!(
        result["infrastructure_error"]
            .as_str()
            .unwrap()
            .contains("receipt")
    );
    assert!(!Path::new(&marker(&r)).exists());
}

#[tokio::test]
async fn a_matching_reused_receipt_with_only_its_digest_corrupted_is_rejected() {
    let r = repo();
    let (url, _) = fake_queen(Mode::BadReusedDigest).await;
    let mut o = opts(&r, Some(url), &marker(&r));
    o.remote_only = true;
    o.result_file = Some(r.dir.join("result.json"));
    assert_eq!(run(&o, &mut Vec::new()).await, 125);
    let result: Value =
        serde_json::from_slice(&std::fs::read(o.result_file.unwrap()).unwrap()).unwrap();
    assert_eq!(result["exit_code"], 125);
    assert!(result["receipt"].is_null());
    assert!(result["host"].is_null());
    assert!(
        result["infrastructure_error"]
            .as_str()
            .unwrap()
            .contains("receipt")
    );
    assert!(!Path::new(&marker(&r)).exists());
}
