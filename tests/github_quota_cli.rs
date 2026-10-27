//! Credential-scoped cooldowns through the production queen CLI wiring.

use crate::support::{Harness, JANITOR_TOKEN, ORG};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::process::{Child, Command};

fn borg_bin() -> String {
    option_env!("CARGO_BIN_EXE_borg")
        .map(str::to_owned)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .and_then(Path::parent)
                .expect("target profile directory")
                .join("borg")
                .to_string_lossy()
                .into_owned()
        })
}

async fn wait_for_passes(
    h: &Harness,
    child: &mut Child,
    url: &str,
    passes: u64,
    diagnostics: &Path,
) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "queen exited before completing its passes: {}",
                std::fs::read_to_string(diagnostics)
                    .unwrap_or_default()
                    .replace(&h.private_pem, "[private key]")
                    .replace(JANITOR_TOKEN, "[janitor token]")
            );
            if let Ok(response) = h
                .http
                .get(format!("{url}/status"))
                .timeout(Duration::from_secs(1))
                .send()
                .await
                && let Ok(status) = response.json::<Value>().await
                && status["requeue"]["passes"].as_u64().unwrap_or(0) >= passes
            {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("queen must finish passes without waiting for the quota reset")
}

async fn exercise_credential_wiring(static_janitor: bool) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queen.db");
    let h = Harness::start_file(json!({}), &db).await;
    h.enroll("d1").await;
    h.preempted(
        "preempted",
        7,
        70,
        time::OffsetDateTime::now_utc().unix_timestamp(),
    )
    .await;
    // The janitor alone reads PR heads; the queen sees the same workflow
    // listing but never touches this endpoint.
    let cleanup_path = format!("/repos/{ORG}/app/pulls/1");
    h.state().http_replies.insert(
        cleanup_path.clone(),
        (
            429,
            vec![("retry-after".into(), "3600".into())],
            "{}".into(),
        ),
    );
    h.state().http_replies.insert(
        format!("/repos/{ORG}/app/actions/runs"),
        (
            200,
            vec![],
            json!({"workflow_runs":[{
                "id":99, "status":"queued", "event":"pull_request",
                "head_sha":"old", "pull_requests":[{"number":1}]
            }]})
            .to_string(),
        ),
    );
    let run_path = format!("/repos/{ORG}/app/actions/runs/7");
    h.state().http_replies.insert(
        run_path.clone(),
        (200, vec![], json!({"status":"completed"}).to_string()),
    );
    let key = dir.path().join("app.pem");
    std::fs::write(&key, &h.private_pem).unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    let mut config = json!({
        "github": {
            "org": ORG, "app_id": "12345", "api_base": h.gh_url,
            "private_key_path": key
        },
        "repos": [format!("{ORG}/app")],
        "db_path": db, "listen": address.to_string(),
        "warm_floors": [], "poll_interval_secs": 1,
        "supersede_janitor": {
            "interval_secs": 1, "dry_run": false
        },
        "preemption": {"after_secs":60, "max_per_pass":1, "min_victim_age_secs":60}
    });
    if static_janitor {
        let token = dir.path().join("janitor.token");
        std::fs::write(&token, JANITOR_TOKEN).unwrap();
        config["supersede_janitor"]["token_path"] = json!(token);
    }
    let path = dir.path().join("queen.json");
    std::fs::write(&path, config.to_string()).unwrap();
    let diagnostics = dir.path().join("queen.stderr");
    let stderr = std::fs::File::create(&diagnostics).unwrap();
    drop(reservation);
    #[allow(
        clippy::disallowed_methods,
        reason = "test fixture: run production CLI with private credentials and isolated configuration"
    )]
    let mut child = Command::new(borg_bin())
        .args(["queen", "run", "--config"])
        .arg(&path)
        .env_remove("BORG_JANITOR_TOKEN")
        .env("RUST_LOG", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let url = format!("http://{address}");
    let first = wait_for_passes(&h, &mut child, &url, 2, &diagnostics).await;
    let requests = h.state().http_requests.clone();
    let status = wait_for_passes(&h, &mut child, &url, 4, &diagnostics).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();

    assert_eq!(h.state().http_requests[&cleanup_path], 1);
    assert_eq!(h.state().http_requests.get(&run_path), None);
    assert_eq!(status["requeue"]["last"]["errors"], 1);
    assert_eq!(status["requeue"]["last"]["terminal"], 0);
    assert!(h.state().reruns.is_empty());
    if static_janitor {
        assert!(status["github_rate_limit"]["retry_at"].is_null());
        assert!(status["last_error"].is_null());
        assert!(status["last_tick_at"].as_i64() > first["last_tick_at"].as_i64());
        let scheduler_path = format!("/repos/{ORG}/app/actions/runs");
        assert!(h.state().http_requests[&scheduler_path] > requests[&scheduler_path]);
    } else {
        assert!(
            status["github_rate_limit"]["retry_at"].as_i64().unwrap()
                > status["now"].as_i64().unwrap()
        );
        assert!(
            status["last_error"]
                .as_str()
                .unwrap()
                .contains("quota cooldown")
        );
        assert_eq!(h.state().http_requests, requests);
    }
}

#[tokio::test]
async fn app_fallback_wires_queen_janitor_and_requeuer_to_one_cooldown() {
    exercise_credential_wiring(false).await;
}

#[tokio::test]
async fn static_janitor_shares_requeuer_cooldown_without_stopping_queen_app() {
    exercise_credential_wiring(true).await;
}
