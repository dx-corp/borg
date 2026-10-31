use crate::support;

use borg::enroll;
use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

const TREE: &str = "0123456789abcdef0123456789abcdef01234567";

#[tokio::test]
async fn registration_requires_approval_and_cannot_acquire_main_or_deploy_scope() {
    let r = Rig::new(json!({})).await;
    let token = enroll::new_token();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let request = borg::client_registration::Request {
        name: "temporary-agent".into(),
        token_hash: enroll::hash_token(&token),
        created_at: now,
    };
    assert_eq!(r.submit_as(&token, &r.unit(json!({}))).await.0, 401);
    borg::client_registration::approve(r.h.queen.store(), &request, 3600, now)
        .await
        .unwrap();
    for scope in ["main", "deploy"] {
        assert_eq!(
            r.submit_as(&token, &r.unit(json!({"scope": scope})))
                .await
                .0,
            403
        );
    }
    assert_eq!(
        r.submit_as(&token, &r.unit(json!({"scope": "pr"}))).await.0,
        201
    );

    let expired = enroll::new_token();
    let request = borg::client_registration::Request {
        name: "expired-agent".into(),
        token_hash: enroll::hash_token(&expired),
        created_at: now - 10,
    };
    borg::client_registration::approve(r.h.queen.store(), &request, 1, now - 10)
        .await
        .unwrap();
    assert_eq!(r.submit_as(&expired, &r.unit(json!({}))).await.0, 401);
}

#[tokio::test]
async fn temporary_client_cannot_read_cancel_or_join_another_owner_and_can_be_revoked() {
    let r = Rig::new(json!({})).await;
    let (_, existing) = r.submit(&r.unit(json!({}))).await;
    let id = existing["unit_id"].as_str().unwrap();
    let token = enroll::new_token();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let request = borg::client_registration::Request {
        name: "bounded-agent".into(),
        token_hash: enroll::hash_token(&token),
        created_at: now,
    };
    borg::client_registration::approve(r.h.queen.store(), &request, 3600, now)
        .await
        .unwrap();
    for suffix in [format!("/v1/units/{id}"), format!("/v1/units/{id}/log")] {
        let response =
            r.h.http
                .get(format!("{}{suffix}", r.h.url))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap();
        assert_eq!(response.status().as_u16(), 403);
    }
    let response =
        r.h.http
            .post(format!("{}/v1/units/{id}/cancel", r.h.url))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
    assert_eq!(response.status().as_u16(), 403);
    assert_eq!(r.submit_as(&token, &r.unit(json!({}))).await.0, 403);
    assert!(
        !r.h.queen
            .store()
            .unit(id)
            .await
            .unwrap()
            .unwrap()
            .cancel_requested
    );

    assert_eq!(
        r.submit_as(&token, &r.unit(json!({"argv": ["cargo", "test"]})))
            .await
            .0,
        201
    );
    assert!(
        borg::client_registration::revoke(r.h.queen.store(), "bounded-agent")
            .await
            .unwrap()
    );
    assert_eq!(r.submit_as(&token, &r.unit(json!({}))).await.0, 401);
    assert!(
        borg::client_registration::approve(r.h.queen.store(), &request, 3600, now)
            .await
            .is_err()
    );
    assert!(
        !borg::client_registration::revoke(r.h.queen.store(), "agent")
            .await
            .unwrap()
    );
    assert_eq!(r.submit(&r.unit(json!({}))).await.0, 200);
}

fn cfg() -> Value {
    json!({"drone_stale_secs": 1_000_000, "units": {"enabled": true, "lease_ttl_secs": 100, "max_attempts": 2}})
}

struct Rig {
    h: Harness,
    client: String,
    base: OffsetDateTime,
}

impl Rig {
    async fn new(extra: Value) -> Self {
        let mut c = cfg();
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            c[k] = v;
        }
        let h = Harness::start(c).await;
        let client = enroll::new_token();
        h.queen
            .store()
            .enroll_client("agent", &enroll::hash_token(&client), 0)
            .await
            .unwrap();
        Self {
            h,
            client,
            base: OffsetDateTime::now_utc(),
        }
    }

    async fn tick(&self, secs: i64) {
        self.h
            .queen
            .tick(self.base + time::Duration::seconds(secs))
            .await
            .unwrap();
    }

    fn unit(&self, extra: Value) -> Value {
        let mut u = json!({"repo": "example-org/app", "tree_sha": TREE, "argv": ["cargo", "check"],
                           "platform": "linux-x86_64", "toolchain": "rust-1.95", "class": "light"});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            u[k] = v;
        }
        u
    }

    async fn submit_as(&self, token: &str, body: &Value) -> (u16, Value) {
        let r = self
            .h
            .http
            .post(format!("{}/v1/units", self.h.url))
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn submit(&self, body: &Value) -> (u16, Value) {
        self.submit_as(&self.client, body).await
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.h
            .http
            .get(format!("{}{path}", self.h.url))
            .bearer_auth(&self.client)
            .send()
            .await
            .unwrap()
    }

    async fn drone(&self, name: &str) -> String {
        let t = self.h.enroll(name).await;
        self.h.heartbeat(&t, &[]).await;
        t
    }

    async fn beat(&self, token: &str, active: &[&str]) -> Value {
        self.h
            .heartbeat_with(token, json!({"active_units": active}))
            .await
    }

    async fn post_log(&self, token: &str, id: &str, offset: u64, data: &str) -> u16 {
        self.h
            .http
            .post(format!("{}/v1/units/{id}/log?offset={offset}", self.h.url))
            .bearer_auth(token)
            .body(data.to_owned())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn post_result(&self, token: &str, id: &str, code: i32) -> u16 {
        self.h
            .http
            .post(format!("{}/v1/units/{id}/result", self.h.url))
            .bearer_auth(token)
            .json(&json!({"exit_code": code, "duration_secs": 12}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}

#[tokio::test]
async fn a_unit_is_placed_run_logged_finished_and_then_served_as_a_receipt() {
    let r = Rig::new(json!({})).await;
    let d1 = r.drone("d1").await;

    let (code, sub) = r.submit(&r.unit(json!({}))).await;
    assert_eq!((code, sub["outcome"].as_str()), (201, Some("new")), "{sub}");
    let id = sub["unit_id"].as_str().unwrap().to_owned();

    // Placement happens on the tick; the drone then hears about it.
    assert!(
        r.beat(&d1, &[]).await["units"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    r.tick(5).await;
    let hb = r.beat(&d1, &[]).await;
    let grant = &hb["units"][0];
    assert_eq!(grant["unit_id"], id.as_str());
    assert_eq!(grant["tree_sha"], TREE);
    assert_eq!(grant["argv"], json!(["cargo", "check"]));
    // A placed unit holds capacity: the drone's free memory in /status drops by
    // the unit's class (light: 2.5 GiB of the 28 GiB budget).
    let status = r.h.status().await;
    let d = status["drones"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "d1")
        .unwrap();
    assert_eq!(d["free_mem_mib"], 28 * 1024 - 2560);
    // Resent until acknowledged; acknowledging starts it.
    assert_eq!(r.beat(&d1, &[]).await["units"].as_array().unwrap().len(), 1);
    assert!(
        r.beat(&d1, &[&id]).await["units"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        r.get(&format!("/v1/units/{id}"))
            .await
            .json::<Value>()
            .await
            .unwrap()["state"],
        "running"
    );

    // The drone streams the log; the client reads it from any offset.
    assert_eq!(r.post_log(&d1, &id, 0, "compiling a\n").await, 200);
    assert_eq!(r.post_log(&d1, &id, 12, "compiling b\n").await, 200);
    assert_eq!(r.post_log(&d1, &id, 99, "gap").await, 409);
    let log = r.get(&format!("/v1/units/{id}/log?from=0")).await;
    assert_eq!(log.headers()["x-borg-next"], "24");
    assert_eq!(log.headers()["x-borg-state"], "running");
    assert!(log.headers().get("x-borg-exit").is_none());
    assert_eq!(log.text().await.unwrap(), "compiling a\ncompiling b\n");
    let tail = r.get(&format!("/v1/units/{id}/log?from=12")).await;
    assert_eq!(tail.text().await.unwrap(), "compiling b\n");

    assert_eq!(r.post_result(&d1, &id, 0).await, 204);
    let done = r.get(&format!("/v1/units/{id}/log?from=24")).await;
    assert_eq!(done.headers()["x-borg-exit"], "0");
    assert_eq!(done.headers()["x-borg-state"], "done");
    let view = r
        .get(&format!("/v1/units/{id}"))
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(view["receipt"]["tree_sha"], TREE);
    assert_eq!(view["receipt"]["drone"], "d1");

    // The same work again is answered from the receipt: nothing runs.
    let (code, again) = r.submit(&r.unit(json!({}))).await;
    assert_eq!((code, again["outcome"].as_str()), (200, Some("receipt")));
    assert_eq!(again["unit_id"], id.as_str());
    assert_eq!(again["receipt"]["exit_code"], 0);
    // A different tree is new work.
    let other = r.unit(json!({"tree_sha": "f".repeat(40)}));
    assert_eq!(r.submit(&other).await.1["outcome"], "new");

    let metrics = r.h.metrics().await;
    assert!(
        metrics.contains("borg_unit_receipt_hits_total 1"),
        "{metrics}"
    );
    assert!(metrics.contains("borg_units{state=\"done\"} 1"));
    assert_eq!(r.h.status().await["units"]["receipt_hits"], 1);
}

#[tokio::test]
async fn identical_submissions_join_the_running_unit() {
    let r = Rig::new(json!({})).await;
    r.drone("d1").await;
    let (_, a) = r.submit(&r.unit(json!({}))).await;
    let (code, b) = r.submit(&r.unit(json!({}))).await;
    assert_eq!((code, b["outcome"].as_str()), (200, Some("joined")));
    assert_eq!(a["unit_id"], b["unit_id"]);
    assert_eq!(
        r.h.metrics()
            .await
            .matches("borg_unit_joins_total 1")
            .count(),
        1
    );
}

#[tokio::test]
async fn placement_honors_platform_quarantine_and_class_capacity() {
    let r = Rig::new(json!({})).await;
    let d1 = r.drone("d1").await;
    // A macOS unit has no macOS drone: it waits.
    let (_, mac) = r
        .submit(&r.unit(json!({"platform": "macos-aarch64"})))
        .await;
    r.tick(1).await;
    let id = mac["unit_id"].as_str().unwrap();
    assert_eq!(
        r.h.queen
            .store()
            .unit(id)
            .await
            .unwrap()
            .unwrap()
            .state
            .as_str(),
        "queued"
    );
    assert!(
        r.beat(&d1, &[]).await["units"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // A macOS drone appears (the harness sends linux unless overridden).
    let m1 = r.h.enroll("m1").await;
    let mut cap = serde_json::to_value(support::capacity()).unwrap();
    cap["os"] = json!("macos");
    cap["arch"] = json!("arm64");
    r.h.heartbeat_with(&m1, json!({"capacity": cap})).await;
    r.tick(2).await;
    let hb = r.h.heartbeat_with(&m1, json!({"capacity": cap})).await;
    assert_eq!(hb["units"][0]["unit_id"], id);

    // A quarantined drone gets nothing.
    r.h.queen
        .store()
        .set_quarantine("d1", Some("test"))
        .await
        .unwrap();
    r.submit(&r.unit(json!({"tree_sha": "e".repeat(40)}))).await;
    r.tick(3).await;
    assert!(
        r.beat(&d1, &[]).await["units"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn units_that_are_never_acknowledged_or_whose_drone_goes_silent_are_retried_then_fail() {
    let r = Rig::new(json!({})).await;
    let d1 = r.drone("d1").await;
    let (_, sub) = r.submit(&r.unit(json!({}))).await;
    let id = sub["unit_id"].as_str().unwrap().to_owned();
    let state = || async { r.h.queen.store().unit(&id).await.unwrap().unwrap() };

    r.tick(1).await; // placed (attempt 1); drone heartbeats stay fresh in this fake time
    assert_eq!(state().await.state.as_str(), "leased");
    // Never acknowledged within lease_ttl (100s): back in the queue, then placed again.
    r.h.heartbeat(&d1, &[]).await;
    r.tick(150).await;
    let u = state().await;
    assert_eq!((u.attempt, u.state.as_str()), (2, "leased"), "{u:?}");
    // Out of attempts (2): the next expiry fails it.
    r.tick(400).await;
    let u = state().await;
    assert_eq!(u.state.as_str(), "failed");
    assert!(u.failure.unwrap().contains("out of attempts"));
    // Failed units are not receipts.
    let (_, again) = r.submit(&r.unit(json!({}))).await;
    assert_eq!(again["outcome"], "new");
}

#[tokio::test]
async fn running_units_time_out_and_cancel_reaches_the_drone() {
    let r = Rig::new(json!({})).await;
    let d1 = r.drone("d1").await;
    let (_, sub) = r.submit(&r.unit(json!({"timeout_secs": 30}))).await;
    let id = sub["unit_id"].as_str().unwrap().to_owned();
    r.tick(1).await;
    r.beat(&d1, &[]).await;
    r.beat(&d1, &[&id]).await;

    // Cancelling a running unit flags it for the drone.
    let c =
        r.h.http
            .post(format!("{}/v1/units/{id}/cancel", r.h.url))
            .bearer_auth(&r.client)
            .send()
            .await
            .unwrap();
    assert_eq!(c.status(), 204);
    assert_eq!(r.beat(&d1, &[&id]).await["cancel_units"], json!([id]));

    // Past its 30 second limit it fails by itself.
    r.tick(60).await;
    let u = r.h.queen.store().unit(&id).await.unwrap().unwrap();
    assert_eq!(u.state.as_str(), "failed");
    assert!(u.failure.unwrap().contains("timed out"));
    // A queued unit cancels at once.
    let (_, q) = r.submit(&r.unit(json!({"tree_sha": "d".repeat(40)}))).await;
    let qid = q["unit_id"].as_str().unwrap();
    r.h.http
        .post(format!("{}/v1/units/{qid}/cancel", r.h.url))
        .bearer_auth(&r.client)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.h.queen
            .store()
            .unit(qid)
            .await
            .unwrap()
            .unwrap()
            .failure
            .as_deref(),
        Some("cancelled")
    );
}

#[tokio::test]
async fn auth_validation_limits_and_the_disabled_default() {
    let r = Rig::new(json!({"units": {"enabled": true, "max_queued_per_client": 1}})).await;
    let d1 = r.drone("d1").await;
    // No token, a drone token and a junk token cannot submit.
    for token in ["", d1.as_str(), "borg_nope"] {
        assert_eq!(
            r.submit_as(token, &r.unit(json!({}))).await.0,
            401,
            "{token:?}"
        );
    }
    // Bad bodies are 400.
    for bad in [
        json!({"tree_sha": "xyz"}),
        json!({"argv": []}),
        json!({"repo": "x"}),
    ] {
        assert_eq!(r.submit(&r.unit(bad)).await.0, 400);
    }
    // The per-client limit: a second distinct unit is refused, but joining is not.
    assert_eq!(r.submit(&r.unit(json!({}))).await.0, 201);
    assert_eq!(
        r.submit(&r.unit(json!({"tree_sha": "c".repeat(40)})))
            .await
            .0,
        429
    );
    assert_eq!(r.submit(&r.unit(json!({}))).await.1["outcome"], "joined");

    // A drone cannot write another drone's unit, and clients cannot post results.
    let (_, sub) = r.submit(&r.unit(json!({}))).await;
    let id = sub["unit_id"].as_str().unwrap();
    r.tick(1).await;
    let d2 = r.drone("d2").await;
    assert_eq!(r.post_log(&d2, id, 0, "x").await, 404);
    assert_eq!(r.post_result(&r.client, id, 0).await, 401);
    assert_eq!(r.get(&format!("/v1/units/{id}")).await.status(), 200);

    // Off by default.
    let off = Harness::start(json!({})).await;
    let resp = off
        .http
        .post(format!("{}/v1/units", off.url))
        .bearer_auth("x")
        .json(&r.unit(json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn a_log_long_poll_returns_as_soon_as_bytes_arrive() {
    let r = Rig::new(json!({})).await;
    let d1 = r.drone("d1").await;
    let (_, sub) = r.submit(&r.unit(json!({}))).await;
    let id = sub["unit_id"].as_str().unwrap().to_owned();
    r.tick(1).await;
    r.beat(&d1, &[]).await;
    r.beat(&d1, &[&id]).await;

    let url = format!("{}/v1/units/{id}/log?from=0&wait=10", r.h.url);
    let http = r.h.http.clone();
    let client = r.client.clone();
    let poll = tokio::spawn(async move {
        http.get(url)
            .bearer_auth(client)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    r.post_log(&d1, &id, 0, "late bytes").await;
    let started = std::time::Instant::now();
    assert_eq!(poll.await.unwrap(), "late bytes");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[tokio::test]
async fn units_on_a_drone_that_goes_silent_return_to_the_queue() {
    let r = Rig::new(json!({"drone_stale_secs": 60})).await;
    let d1 = r.drone("d1").await;
    let (_, sub) = r.submit(&r.unit(json!({}))).await;
    let id = sub["unit_id"].as_str().unwrap().to_owned();
    r.tick(1).await;
    r.beat(&d1, &[]).await;
    r.beat(&d1, &[&id]).await;
    assert_eq!(
        r.h.queen
            .store()
            .unit(&id)
            .await
            .unwrap()
            .unwrap()
            .state
            .as_str(),
        "running"
    );
    // No heartbeat for longer than drone_stale_secs: the unit is requeued and,
    // with no fresh drone, waits.
    r.tick(200).await;
    let u = r.h.queen.store().unit(&id).await.unwrap().unwrap();
    assert_eq!((u.state.as_str(), u.drone, u.attempt), ("queued", None, 1));
}

impl Rig {
    async fn put_bundle(&self, token: &str, id: &str, data: Vec<u8>) -> u16 {
        self.h
            .http
            .put(format!("{}/v1/units/{id}/bundle", self.h.url))
            .bearer_auth(token)
            .body(data)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn get_bundle(&self, token: &str, id: &str) -> (u16, Vec<u8>) {
        let r = self
            .h
            .http
            .get(format!("{}/v1/units/{id}/bundle", self.h.url))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.bytes().await.unwrap().to_vec())
    }
}

#[tokio::test]
async fn a_unit_waits_for_its_bundle_then_the_drone_fetches_it() {
    let r = Rig::new(json!({"units": {"enabled": true, "max_bundle_bytes": 1000}})).await;
    let d1 = r.drone("d1").await;
    let (_, sub) = r.submit(&r.unit(json!({"bundle": true}))).await;
    let id = sub["unit_id"].as_str().unwrap().to_owned();

    // Not placeable until the bundle arrives.
    r.tick(1).await;
    assert!(
        r.beat(&d1, &[]).await["units"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let u = r
        .get(&format!("/v1/units/{id}"))
        .await
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(
        (u["state"].as_str(), u["awaiting_bundle"].as_bool()),
        (Some("queued"), Some(true))
    );

    // Size cap, a non-owner, and an empty body are refused.
    assert_eq!(r.put_bundle(&r.client, &id, vec![7; 2000]).await, 413);
    let other = enroll::new_token();
    r.h.queen
        .store()
        .enroll_client("other", &enroll::hash_token(&other), 0)
        .await
        .unwrap();
    assert_eq!(r.put_bundle(&other, &id, b"x".to_vec()).await, 404);
    assert_eq!(r.put_bundle(&r.client, &id, vec![]).await, 400);
    assert_eq!(r.put_bundle("", &id, b"x".to_vec()).await, 401);

    let bundle = b"# v2 git bundle\nfake".to_vec();
    assert_eq!(r.put_bundle(&r.client, &id, bundle.clone()).await, 204);
    r.tick(2).await;
    let grant = &r.beat(&d1, &[]).await["units"][0];
    assert_eq!(grant["unit_id"], id.as_str());
    assert_eq!(grant["bundle_bytes"], bundle.len());

    // Only the unit's drone can fetch it.
    assert_eq!(r.get_bundle(&d1, &id).await, (200, bundle.clone()));
    let d2 = r.drone("d2").await;
    assert_eq!(r.get_bundle(&d2, &id).await.0, 404);
    assert_eq!(r.get_bundle(&r.client, &id).await.0, 401);
    // Once placed, a late upload is refused.
    assert_eq!(r.put_bundle(&r.client, &id, bundle).await, 409);
}

#[tokio::test]
async fn a_bundle_that_never_arrives_fails_the_unit_and_bundles_go_with_their_unit() {
    let r = Rig::new(json!({})).await;
    let d1 = r.drone("d1").await;
    let (_, a) = r.submit(&r.unit(json!({"bundle": true}))).await;
    let a = a["unit_id"].as_str().unwrap().to_owned();
    r.tick(30).await;
    assert_eq!(
        r.h.queen
            .store()
            .unit(&a)
            .await
            .unwrap()
            .unwrap()
            .state
            .as_str(),
        "queued"
    );
    r.tick(100).await; // bundle_wait_secs is 60
    let u = r.h.queen.store().unit(&a).await.unwrap().unwrap();
    assert_eq!(u.state.as_str(), "failed");
    assert!(u.failure.unwrap().contains("bundle never arrived"));

    // Cancelling deletes the bundle at once.
    let (_, b) = r
        .submit(&r.unit(json!({"bundle": true, "tree_sha": "e".repeat(40)})))
        .await;
    let b = b["unit_id"].as_str().unwrap().to_owned();
    assert_eq!(r.put_bundle(&r.client, &b, b"bundle".to_vec()).await, 204);
    r.h.http
        .post(format!("{}/v1/units/{b}/cancel", r.h.url))
        .bearer_auth(&r.client)
        .send()
        .await
        .unwrap();
    assert!(r.h.queen.store().unit_bundle(&b).await.unwrap().is_none());

    // A finished unit keeps its bundle for the life of its receipt, then both go.
    let (_, c) = r
        .submit(&r.unit(json!({"bundle": true, "tree_sha": "d".repeat(40)})))
        .await;
    let c = c["unit_id"].as_str().unwrap().to_owned();
    r.put_bundle(&r.client, &c, b"bundle".to_vec()).await;
    r.tick(101).await;
    r.beat(&d1, &[]).await;
    r.beat(&d1, &[&c]).await;
    assert_eq!(r.post_result(&d1, &c, 0).await, 204);
    assert!(r.h.queen.store().unit_bundle(&c).await.unwrap().is_some());
    r.tick(8 * 86_400).await; // past keep_secs (7 days)
    assert!(r.h.queen.store().unit(&c).await.unwrap().is_none());
    assert!(r.h.queen.store().unit_bundle(&c).await.unwrap().is_none());
}

fn tree(c: char) -> String {
    c.to_string().repeat(40)
}

async fn state_of(r: &Rig, id: &str) -> String {
    r.h.queen
        .store()
        .unit(id)
        .await
        .unwrap()
        .unwrap()
        .state
        .as_str()
        .to_owned()
}

async fn deploy_drone(r: &Rig, name: &str) -> String {
    let t = r.h.enroll(name).await;
    r.h.heartbeat_with(&t, json!({"trust": "deploy"})).await;
    t
}

async fn scoped_client(r: &Rig, name: &str, max: borg::classes::Scope) -> String {
    let tok = enroll::new_token();
    r.h.queen
        .store()
        .enroll_client_scoped(name, &enroll::hash_token(&tok), max, 0)
        .await
        .unwrap();
    tok
}

#[tokio::test]
async fn scope_round_trips_and_defaults_to_pr() {
    let r = Rig::new(json!({})).await;
    r.drone("d1").await;
    let (_, a) = r.submit(&r.unit(json!({}))).await;
    let u = r
        .get(&format!("/v1/units/{}", a["unit_id"].as_str().unwrap()))
        .await;
    assert_eq!(u.json::<Value>().await.unwrap()["scope"], "pr");

    let tok = scoped_client(&r, "gate", borg::classes::Scope::Main).await;
    let body = r.unit(json!({"scope": "main", "tree_sha": tree('1')}));
    let (code, sub) = r.submit_as(&tok, &body).await;
    assert_eq!(code, 201, "{sub}");
    let u = r
        .get(&format!("/v1/units/{}", sub["unit_id"].as_str().unwrap()))
        .await;
    assert_eq!(u.json::<Value>().await.unwrap()["scope"], "main");
    assert_eq!(
        r.h.queen.store().client_max_scope("gate").await.unwrap(),
        borg::classes::Scope::Main
    );
}

#[tokio::test]
async fn a_unit_above_the_clients_maximum_scope_is_refused() {
    let r = Rig::new(json!({})).await;
    let hook = scoped_client(&r, "hook", borg::classes::Scope::Pr).await;
    let resp =
        r.h.http
            .post(format!("{}/v1/units", r.h.url))
            .bearer_auth(&hook)
            .json(&r.unit(json!({"scope": "deploy"})))
            .send()
            .await
            .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
    let msg = resp.text().await.unwrap();
    assert!(
        msg.contains("up to scope pr") && msg.contains("asked for deploy"),
        "{msg}"
    );
    let (code, _) = r.submit_as(&hook, &r.unit(json!({"scope": "main"}))).await;
    assert_eq!(code, 403);
    let (code, _) = r.submit_as(&hook, &r.unit(json!({"scope": "pr"}))).await;
    assert_eq!(code, 201);
}

#[tokio::test]
async fn one_free_slot_goes_to_deploy_then_main_then_pr() {
    let r = Rig::new(json!({})).await;
    let tok = scoped_client(&r, "gate", borg::classes::Scope::Deploy).await;
    let d = deploy_drone(&r, "d1").await;
    // Oldest first: pr, then main, then deploy. A heavy unit fills the drone.
    let mut ids = Vec::new();
    for (scope, c) in [("pr", '1'), ("main", '2'), ("deploy", '3')] {
        let body = r.unit(json!({"scope": scope, "class": "heavy", "tree_sha": tree(c)}));
        let (code, sub) = r.submit_as(&tok, &body).await;
        assert_eq!(code, 201, "{sub}");
        ids.push(sub["unit_id"].as_str().unwrap().to_owned());
    }
    for (n, want) in [2usize, 1, 0].into_iter().enumerate() {
        r.tick(10 + i64::try_from(n).unwrap()).await;
        let grants = r.beat(&d, &[]).await["units"].clone();
        assert_eq!(grants.as_array().unwrap().len(), 1, "round {n}: {grants}");
        assert_eq!(grants[0]["unit_id"], ids[want].as_str(), "round {n}");
        r.beat(&d, &[&ids[want]]).await;
        assert_eq!(r.post_result(&d, &ids[want], 0).await, 204);
    }
}

#[tokio::test]
async fn a_deploy_unit_is_not_placed_on_a_shared_drone() {
    let r = Rig::new(json!({})).await;
    let tok = scoped_client(&r, "gate", borg::classes::Scope::Deploy).await;
    let shared = r.drone("shared").await;
    let (_, dep) = r.submit_as(&tok, &r.unit(json!({"scope": "deploy"}))).await;
    let dep = dep["unit_id"].as_str().unwrap().to_owned();
    let (_, main) = r
        .submit_as(
            &tok,
            &r.unit(json!({"scope": "main", "tree_sha": tree('4')})),
        )
        .await;
    let main = main["unit_id"].as_str().unwrap().to_owned();
    r.tick(5).await;
    assert_eq!(state_of(&r, &dep).await, "queued");
    // A shared drone does run main work, so only the deploy unit waits.
    let grants = r.beat(&shared, &[]).await["units"].clone();
    assert_eq!(grants.as_array().unwrap().len(), 1);
    assert_eq!(grants[0]["unit_id"], main.as_str());

    // A deploy-trust drone takes it.
    let d = deploy_drone(&r, "dep").await;
    r.tick(6).await;
    assert_eq!(state_of(&r, &dep).await, "leased");
    assert_eq!(r.beat(&d, &[]).await["units"][0]["unit_id"], dep.as_str());

    // A pr unit never runs on a deploy drone: with only one, it waits.
    let r2 = Rig::new(json!({})).await;
    let d2 = deploy_drone(&r2, "dep").await;
    let (_, pr) = r2.submit(&r2.unit(json!({}))).await;
    r2.tick(5).await;
    assert_eq!(
        state_of(&r2, pr["unit_id"].as_str().unwrap()).await,
        "queued"
    );
    assert!(
        r2.beat(&d2, &[]).await["units"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_newest_unit_with_a_supersede_key_wins() {
    let r = Rig::new(json!({})).await;
    let d = r.drone("d1").await;
    let sub = |c: char, extra: Value| {
        let mut b = r.unit(json!({"tree_sha": tree(c)}));
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            b[k] = v;
        }
        b
    };
    let id = |v: &Value| v["unit_id"].as_str().unwrap().to_owned();

    // A record unit, then running (placed and acknowledged), queued, and
    // other keys. Submitting a record unit with a key supersedes older
    // non-record units too, so it goes first.
    let (_, record) = r
        .submit(&sub('3', json!({"supersede_key": "pr-7", "record": true})))
        .await;
    let record = id(&record);
    let (_, running) = r.submit(&sub('1', json!({"supersede_key": "pr-7"}))).await;
    let running = id(&running);
    r.tick(1).await;
    r.beat(&d, &[]).await;
    r.beat(&d, &[&running]).await;
    assert_eq!(state_of(&r, &running).await, "running");
    let (_, other) = r.submit(&sub('4', json!({"supersede_key": "pr-8"}))).await;
    let other = id(&other);
    let (_, plain) = r.submit(&sub('5', json!({}))).await;
    let plain = id(&plain);

    let (code, newest) = r.submit(&sub('6', json!({"supersede_key": "pr-7"}))).await;
    assert_eq!(code, 201, "{newest}");
    assert_eq!(newest["superseded"], json!([running]));
    let rec = r.h.queen.store().unit(&record).await.unwrap().unwrap();
    assert!(!rec.cancel_requested && rec.state.as_str() != "failed");
    assert_eq!(state_of(&r, &other).await, "queued");
    assert_eq!(state_of(&r, &plain).await, "queued");

    // The running one is told to stop on the drone's next heartbeat.
    let hb = r.beat(&d, &[&running]).await;
    assert_eq!(hb["cancel_units"], json!([running]));

    // A queued unit is cancelled at once by the next submission with the key.
    let (_, third) = r.submit(&sub('2', json!({"supersede_key": "pr-7"}))).await;
    assert_eq!(third["superseded"], json!([id(&newest)]));
    let q = r.h.queen.store().unit(&id(&newest)).await.unwrap().unwrap();
    assert_eq!(q.state.as_str(), "failed");
    assert_eq!(q.failure.unwrap(), format!("superseded by {}", id(&third)));

    assert_eq!(r.h.status().await["units"]["superseded"], 2);
    assert!(
        r.h.metrics()
            .await
            .contains("borg_units_superseded_total 2")
    );

    // Another client's units with the same key are left alone.
    let tok = scoped_client(&r, "other", borg::classes::Scope::Pr).await;
    let (_, theirs) = r
        .submit_as(&tok, &sub('7', json!({"supersede_key": "pr-9"})))
        .await;
    let (_, mine) = r.submit(&sub('8', json!({"supersede_key": "pr-9"}))).await;
    assert!(mine["superseded"].as_array().unwrap().is_empty());
    assert_eq!(state_of(&r, &id(&theirs)).await, "queued");

    // The key is bounded.
    let (code, _) = r
        .submit(&sub('9', json!({"supersede_key": "k".repeat(129)})))
        .await;
    assert_eq!(code, 400);
}

async fn retire_ci(r: &Rig, count: usize, now: i64) {
    let leases = r.h.queen.store().active_leases().await.unwrap();
    for lease in leases.into_iter().take(count) {
        r.h.queen
            .store()
            .set_state(&lease.id, borg::store::LeaseStatus::Done, now)
            .await
            .unwrap();
    }
}

fn sustained_ci(r: &Rig) {
    for id in 1..=20 {
        r.h.queue_job(id, "borg-ci-light-pr", 0);
    }
}

#[tokio::test]
async fn unit_share_survives_sustained_ci_and_rotates_clients_across_ticks() {
    let r = Rig::new(json!({})).await;
    let d = r.drone("d1").await;
    let other = scoped_client(&r, "other", borg::classes::Scope::Pr).await;
    sustained_ci(&r);
    for n in 0..4 {
        for (token, name) in [(&r.client, "agent"), (&other, "other")] {
            let (_, sub) = r
                .submit_as(
                    token,
                    &r.unit(json!({"argv": ["job", n.to_string(), name]})),
                )
                .await;
            assert_eq!(sub["outcome"], "new");
        }
    }
    let mut served = Vec::new();
    for round in 0..4 {
        r.tick(10 + round * 2).await;
        let units =
            r.h.queen
                .store()
                .units_in(&[
                    borg::units::UnitState::Leased,
                    borg::units::UnitState::Running,
                ])
                .await
                .unwrap();
        // 25% of 16 vCPU = two light units; CI consumes the remaining six.
        assert_eq!(units.len(), 2);
        assert_eq!(r.h.queen.store().active_leases().await.unwrap().len(), 6);
        assert_ne!(
            units[0].client, units[1].client,
            "both clients receive their share"
        );
        if round == 0 {
            // Held units count toward the next tick's budget.
            r.tick(11).await;
            assert_eq!(
                r.h.queen
                    .store()
                    .units_in(&[
                        borg::units::UnitState::Leased,
                        borg::units::UnitState::Running
                    ])
                    .await
                    .unwrap()
                    .len(),
                2
            );
        }
        for unit in units {
            served.push(unit.client.clone());
            r.beat(&d, &[&unit.id]).await;
            assert_eq!(r.post_result(&d, &unit.id, 0).await, 204);
        }
        retire_ci(&r, usize::MAX, r.base.unix_timestamp() + 11 + round * 2).await;
    }
    assert_eq!(served.iter().filter(|c| c.as_str() == "agent").count(), 4);
    assert_eq!(served.iter().filter(|c| c.as_str() == "other").count(), 4);
}

#[tokio::test]
async fn zero_share_keeps_ci_first_and_idle_ci_borrows_the_unit_share() {
    for percent in [0, 25] {
        let r =
            Rig::new(json!({"units": {"enabled": true, "capacity_share_percent": percent}})).await;
        r.drone("d1").await;
        sustained_ci(&r);
        // No unit demand reserves no capacity, even with the feature enabled.
        r.tick(5).await;
        assert_eq!(r.h.queen.store().active_leases().await.unwrap().len(), 8);
        let (_, sub) = r.submit(&r.unit(json!({}))).await;
        retire_ci(&r, usize::MAX, r.base.unix_timestamp() + 6).await;
        r.tick(10).await;
        assert_eq!(
            state_of(&r, sub["unit_id"].as_str().unwrap()).await,
            if percent == 0 { "queued" } else { "leased" }
        );
        assert_eq!(
            r.h.queen.store().active_leases().await.unwrap().len(),
            if percent == 0 { 8 } else { 7 }
        );
    }
}

#[tokio::test]
async fn pending_ci_runners_do_not_absorb_work_unit_demand() {
    let r = Rig::new(json!({"units": {"enabled": true, "capacity_share_percent": 0}})).await;
    r.drone("d1").await;
    r.h.queue_job(1, "borg-ci-light-pr", 0);
    r.tick(5).await;
    let (_, sub) = r.submit(&r.unit(json!({}))).await;
    r.tick(10).await;
    assert_eq!(
        state_of(&r, sub["unit_id"].as_str().unwrap()).await,
        "leased"
    );
    assert_eq!(r.h.queen.store().active_leases().await.unwrap().len(), 1);
}

#[tokio::test]
async fn aging_reserves_contiguous_room_and_a_heavy_unit_can_cross_the_share() {
    let r = Rig::new(json!({"starvation_after_secs": 30})).await;
    r.drone("d1").await;
    sustained_ci(&r);
    r.tick(1).await;
    let (_, sub) = r.submit(&r.unit(json!({"class": "heavy"}))).await;
    let id = sub["unit_id"].as_str().unwrap();
    r.tick(5).await;
    assert_eq!(state_of(&r, id).await, "queued");
    retire_ci(&r, 1, r.base.unix_timestamp() + 35).await;
    r.tick(40).await;
    assert_eq!(state_of(&r, id).await, "queued");
    assert_eq!(
        r.h.queen.store().active_leases().await.unwrap().len(),
        7,
        "an aged heavy unit prevents CI refilling its reserved host"
    );
    assert!(
        r.h.status().await["reserved_for"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["class"] == "heavy")
    );
    retire_ci(&r, usize::MAX, r.base.unix_timestamp() + 41).await;
    r.tick(45).await;
    assert_eq!(
        state_of(&r, id).await,
        "leased",
        "24 GiB unit exceeds the 7 GiB share but remains indivisible"
    );
    assert_eq!(r.h.queen.store().active_leases().await.unwrap().len(), 1);
}

#[tokio::test]
async fn active_units_reduce_client_priority_even_across_toolchains() {
    let r = Rig::new(json!({"units": {"enabled": true, "capacity_share_percent": 50}})).await;
    let d = r.drone("d1").await;
    let other = scoped_client(&r, "other", borg::classes::Scope::Pr).await;
    sustained_ci(&r);
    let mut occupied = Vec::new();
    for n in 0..4 {
        let (_, sub) = r
            .submit(&r.unit(json!({"argv": ["job", n.to_string()], "rust_toolchain": "1.95.0"})))
            .await;
        if n < 2 {
            let id = sub["unit_id"].as_str().unwrap();
            assert!(
                r.h.queen
                    .store()
                    .lease_unit(id, "d1", r.base.unix_timestamp())
                    .await
                    .unwrap()
            );
            occupied.push(id.to_owned());
        }
        r.submit_as(
            &other,
            &r.unit(json!({"argv": ["other", n.to_string()], "rust_toolchain": "1.96.0"})),
        )
        .await;
    }
    r.beat(&d, &occupied.iter().map(String::as_str).collect::<Vec<_>>())
        .await;
    r.tick(5).await;
    let units =
        r.h.queen
            .store()
            .units_in(&[
                borg::units::UnitState::Leased,
                borg::units::UnitState::Running,
            ])
            .await
            .unwrap();
    assert_eq!(units.len(), 4);
    assert_eq!(units.iter().filter(|u| u.client == "agent").count(), 2);
    assert_eq!(
        units.iter().filter(|u| u.client == "other").count(),
        2,
        "the less-loaded client wins despite its lexically later toolchain"
    );
    assert_eq!(r.h.queen.store().active_leases().await.unwrap().len(), 4);
}

#[tokio::test]
async fn unit_share_never_overrides_owner_caps_or_unavailable_bids() {
    let r = Rig::new(json!({"starvation_after_secs": 1})).await;
    let d = r.h.enroll("d1").await;
    r.h.heartbeat_with(&d, json!({"caps": {"max_vcpu": 4, "max_leases": 2},
        "bids": [{"class":"light", "scope":"pr", "available_slots": 1, "expires_at": r.base.unix_timestamp() + 1000}]})).await;
    sustained_ci(&r);
    let (_, heavy) = r.submit(&r.unit(json!({"class": "heavy"}))).await;
    for n in 0..3 {
        r.submit(&r.unit(json!({"argv": ["light", n.to_string()]})))
            .await;
    }
    r.tick(5).await;
    assert_eq!(
        state_of(&r, heavy["unit_id"].as_str().unwrap()).await,
        "queued"
    );
    let units =
        r.h.queen
            .store()
            .units_in(&[borg::units::UnitState::Leased])
            .await
            .unwrap();
    assert_eq!(
        units.len(),
        1,
        "a heartbeat bid slot is shared by both allocation passes"
    );
    assert!(r.h.queen.store().active_leases().await.unwrap().is_empty());
    assert!(
        r.h.status().await["reserved_for"]
            .as_array()
            .unwrap()
            .is_empty(),
        "an impossible heavy unit cannot reserve this capped host"
    );
}

#[tokio::test]
async fn deploy_ci_retains_priority_over_the_unit_share() {
    let r = Rig::new(json!({})).await;
    deploy_drone(&r, "deploy").await;
    let client = scoped_client(&r, "gate", borg::classes::Scope::Main).await;
    let (_, sub) = r
        .submit_as(&client, &r.unit(json!({"scope":"main", "class":"heavy"})))
        .await;
    r.h.queue_job(1, "borg-ci-heavy-deploy", 0);
    r.tick(5).await;
    assert_eq!(
        state_of(&r, sub["unit_id"].as_str().unwrap()).await,
        "queued"
    );
    let leases = r.h.queen.store().active_leases().await.unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].scope, borg::classes::Scope::Deploy);
}

#[tokio::test]
async fn one_slot_share_rotates_clients_without_toolchain_lexical_priority() {
    let r = Rig::new(json!({})).await;
    let d = r.drone("d1").await;
    let other = scoped_client(&r, "other", borg::classes::Scope::Pr).await;
    sustained_ci(&r);
    for n in 0..2 {
        for (token, toolchain) in [(&r.client, "1.96.0"), (&other, "1.95.0")] {
            r.submit_as(token, &r.unit(json!({"class":"heavy", "argv":["job",n.to_string(),toolchain], "rust_toolchain":toolchain}))).await;
        }
    }
    let mut clients = Vec::new();
    for round in 0..4 {
        r.tick(5 + round).await;
        let units =
            r.h.queen
                .store()
                .units_in(&[borg::units::UnitState::Leased])
                .await
                .unwrap();
        assert_eq!(units.len(), 1);
        clients.push(units[0].client.clone());
        r.beat(&d, &[&units[0].id]).await;
        assert_eq!(r.post_result(&d, &units[0].id, 0).await, 204);
        retire_ci(&r, usize::MAX, r.base.unix_timestamp() + 5 + round).await;
    }
    assert_ne!(clients[0], clients[1]);
    assert_eq!(clients[0], clients[2]);
    assert_eq!(clients[1], clients[3]);
}

#[tokio::test]
async fn aging_does_not_reserve_a_host_missing_the_units_toolchain() {
    let r = Rig::new(json!({"starvation_after_secs":1,
        "env_contract":{"enabled":true,"require_report":true,
            "checks":{"compiler_cache":"off","toolchain":"required","cache_store":"off","resources":"off","tools":"off"}}})).await;
    let d = r.h.enroll("d1").await;
    r.h.heartbeat_with(
        &d,
        json!({"env_facts":{"observed_at":r.base.unix_timestamp(),
        "toolchains":[{"channel":"1.95.0","rustc_version":"1.95.0"}]}}),
    )
    .await;
    sustained_ci(&r);
    // Age only the unit, so an empty reservation set specifically proves
    // that its missing toolchain cannot hold capacity from young CI demand.
    let id = submit_at(
        &r,
        "agent",
        json!({"class":"heavy","rust_toolchain":"1.96.0"}),
        r.base.unix_timestamp() - 10,
    )
    .await;
    r.tick(0).await;
    assert_eq!(state_of(&r, &id).await, "queued");
    assert!(
        r.h.status().await["reserved_for"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        r.h.queen.store().active_leases().await.unwrap().len(),
        8,
        "ineligible work holds no CI capacity"
    );
    // Once CI itself ages, its residual demand may legitimately reserve the
    // host. That reservation must be light CI, never the blocked heavy unit.
    r.tick(5).await;
    assert_eq!(state_of(&r, &id).await, "queued");
    assert_eq!(
        r.h.status().await["reserved_for"],
        json!([{"drone_id":"d1","class":"light","scope":"pr"}])
    );
    assert_eq!(r.h.queen.store().active_leases().await.unwrap().len(), 8);
    let report = explanation(&r, &id).await;
    assert!(
        report
            .history
            .iter()
            .all(|row| row.decision.reservation.is_none())
    );
}

#[tokio::test]
async fn per_unit_fairness_preserves_backlog_bid_pressure() {
    let r = Rig::new(json!({})).await;
    let d = r.h.enroll("d1").await;
    r.h.heartbeat_with(
        &d,
        json!({"bids":[{"class":"light","scope":"pr",
        "available_slots":8,"min_pressure":500,"expires_at":r.base.unix_timestamp()+1000}]}),
    )
    .await;
    for n in 0..8 {
        r.submit(&r.unit(json!({"argv":["job",n.to_string()]})))
            .await;
    }
    sustained_ci(&r);
    r.tick(5).await;
    assert_eq!(
        r.h.queen
            .store()
            .units_in(&[borg::units::UnitState::Leased])
            .await
            .unwrap()
            .len(),
        2,
        "eight queued units create pressure500; selecting one fairly must retain the backlog pressure"
    );
    assert_eq!(r.h.queen.store().active_leases().await.unwrap().len(), 6);
}

#[tokio::test]
async fn unit_share_accounts_for_owner_lease_slots_across_platforms() {
    let r = Rig::new(json!({})).await;
    let linux = r.h.enroll("linux").await;
    r.h.heartbeat_with(&linux, json!({"caps":{"max_leases":1}}))
        .await;
    let mac = r.h.enroll("mac").await;
    let mut cap = serde_json::to_value(support::capacity()).unwrap();
    cap["os"] = json!("macos");
    cap["arch"] = json!("aarch64");
    r.h.heartbeat_with(&mac, json!({"capacity":cap,"caps":{"max_leases":1}}))
        .await;
    r.submit(&r.unit(json!({"platform":"linux-x86_64"}))).await;
    r.submit(&r.unit(json!({"platform":"macos-aarch64"}))).await;
    sustained_ci(&r);
    r.tick(5).await;
    assert_eq!(
        r.h.queen
            .store()
            .units_in(&[borg::units::UnitState::Leased])
            .await
            .unwrap()
            .len(),
        1,
        "25 percent of two owner-capped slots rounds to one, even with spare CPU and memory"
    );
    assert_eq!(r.h.queen.store().active_leases().await.unwrap().len(), 1);
}

async fn explanation(r: &Rig, key: &str) -> borg::explain::Explanation {
    r.h.http
        .get(format!("{}/explain", r.h.url))
        .query(&[("key", key)])
        .bearer_auth(&r.client)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn queen_automatically_retains_unit_and_runner_decisions_after_persistence() {
    use borg::explain::{PlacementKind, PlacementOutcome};
    let r = Rig::new(json!({})).await;
    r.drone("d1").await;
    let (_, sub) = r.submit(&r.unit(json!({}))).await;
    let (_, wrong_platform) = r.submit(&r.unit(json!({"platform":"macos-aarch64"}))).await;
    r.h.queue_job(1, "borg-ci-light-pr", 0);
    r.tick(5).await;
    let success = explanation(&r, sub["unit_id"].as_str().unwrap()).await;
    assert!(!success.history.is_empty());
    assert_eq!(success.history[0].kind, PlacementKind::Unit);
    assert_eq!(success.history[0].outcome, PlacementOutcome::Granted);
    assert_eq!(success.history[0].decision.winner.as_deref(), Some("d1"));
    let unplaced = explanation(&r, wrong_platform["unit_id"].as_str().unwrap()).await;
    assert!(!unplaced.history.is_empty());
    assert_eq!(unplaced.history[0].outcome, PlacementOutcome::Unplaced);
    assert!(unplaced.history[0].decision.winner.is_none());
    assert!(unplaced.history[0].decision.candidates.is_empty());
    let runner = explanation(&r, "borg-ci-light-pr").await;
    assert!(!runner.history.is_empty());
    assert_eq!(runner.history[0].kind, PlacementKind::Runner);
    assert_eq!(runner.history[0].outcome, PlacementOutcome::Granted);
}

#[tokio::test]
async fn a_selected_runner_is_failed_in_history_when_minting_fails() {
    let r = Rig::new(json!({})).await;
    r.drone("d1").await;
    r.h.queue_job(1, "borg-ci-light-pr", 0);
    r.h.state().fail_mint = true;
    r.tick(5).await;
    assert!(r.h.queen.store().active_leases().await.unwrap().is_empty());
    let report = explanation(&r, "borg-ci-light-pr").await;
    assert_eq!(report.history.len(), 1);
    assert_eq!(
        report.history[0].outcome,
        borg::explain::PlacementOutcome::Failed
    );
    assert_eq!(report.history[0].decision.winner.as_deref(), Some("d1"));
}

#[tokio::test]
async fn blocked_unit_history_contains_effective_typed_checks_without_raw_environment() {
    use borg::envcontract::{CheckKind, CheckState};
    let r = Rig::new(json!({"env_contract":{"enabled":true,"require_report":true,
        "checks":{"compiler_cache":"off","toolchain":"required","cache_store":"off","resources":"off","tools":"off"}}})).await;
    let d = r.h.enroll("d1").await;
    r.h.heartbeat_with(&d,json!({"env_facts":{"observed_at":r.base.unix_timestamp(),
        "rustc_wrapper":"private-path-should-not-appear", "toolchains":[{"channel":"1.95.0","rustc_version":"private-version-should-not-appear"}]}})).await;
    let (_, sub) = r.submit(&r.unit(json!({"rust_toolchain":"1.96.0"}))).await;
    r.tick(5).await;
    let report = explanation(&r, sub["unit_id"].as_str().unwrap()).await;
    assert_eq!(
        report.history[0].outcome,
        borg::explain::PlacementOutcome::Unplaced
    );
    let candidate = &report.history[0].decision.candidates[0];
    assert_eq!(
        candidate.environment_checks.get(&CheckKind::Toolchain),
        Some(&CheckState::Fail)
    );
    assert!(
        candidate
            .rejection_reasons
            .contains(&borg::allocator::Rejection::Environment)
    );
    let encoded = serde_json::to_string(&report).unwrap();
    assert!(!encoded.contains("private-path-should-not-appear"));
    assert!(!encoded.contains("private-version-should-not-appear"));
}

#[tokio::test]
async fn unit_history_uses_effective_capability_requirements_and_freshness() {
    use borg::envcontract::{Capability, CheckState};
    let r = Rig::new(json!({"env_contract":{"enabled":true,"facts_max_age_secs":2,
        "checks":{"compiler_cache":"off","toolchain":"off","cache_store":"off","resources":"off","tools":"off"},
        "required_capabilities":["sandbox"],"classes":{"light":{"required_capabilities":["docker"]}}}})).await;
    let d = r.h.enroll("d1").await;
    r.h.heartbeat_with(
        &d,
        json!({"env_facts":{"observed_at":r.base.unix_timestamp(),"capabilities":{
        "sandbox":{"observed_at":r.base.unix_timestamp(),"state":"fail"},
        "docker":{"observed_at":r.base.unix_timestamp(),"state":"pass"}}}}),
    )
    .await;
    let (_, sub) = r.submit(&r.unit(json!({}))).await;
    r.tick(5).await;
    let report = explanation(&r, sub["unit_id"].as_str().unwrap()).await;
    assert_eq!(
        report.history[0].outcome,
        borg::explain::PlacementOutcome::Unplaced
    );
    let candidate = &report.history[0].decision.candidates[0];
    assert_eq!(candidate.capability_checks.len(), 1);
    assert_eq!(
        candidate.capability_checks.get(&Capability::Docker),
        Some(&CheckState::Unknown)
    );
    assert!(
        !candidate
            .capability_checks
            .contains_key(&Capability::Sandbox),
        "a class replacement excludes the global requirement from diagnostics too"
    );
}

#[tokio::test]
async fn leased_units_leave_the_backlog_before_the_next_bid_threshold_check() {
    let r = Rig::new(json!({})).await;
    let d = r.h.enroll("d1").await;
    r.h.heartbeat_with(
        &d,
        json!({"bids":[{"class":"light","scope":"pr",
        "available_slots":8,"min_pressure":250,"expires_at":r.base.unix_timestamp()+1000}]}),
    )
    .await;
    for n in 0..4 {
        r.submit(&r.unit(json!({"argv":["job",n.to_string()]})))
            .await;
    }
    sustained_ci(&r);
    r.tick(5).await;
    assert_eq!(
        r.h.queen
            .store()
            .units_in(&[borg::units::UnitState::Leased])
            .await
            .unwrap()
            .len(),
        1,
        "initial pressure4*500/8=250 admits one; remaining3*500/7=214 is below the bid threshold"
    );
    assert_eq!(r.h.queen.store().active_leases().await.unwrap().len(), 7);
    let queued =
        r.h.queen
            .store()
            .units_in(&[borg::units::UnitState::Queued])
            .await
            .unwrap();
    assert_eq!(queued.len(), 3);
    let report = explanation(&r, &queued[0].id).await;
    assert!(report.history.iter().any(|row| row.decision.pressure == 214
        && row.outcome == borg::explain::PlacementOutcome::Unplaced));
}

#[tokio::test]
async fn young_deploy_ci_keeps_priority_when_main_ci_has_starved() {
    let r = Rig::new(json!({"starvation_after_secs":30})).await;
    deploy_drone(&r, "deploy").await;
    let client = scoped_client(&r, "gate", borg::classes::Scope::Main).await;
    let (_, unit) = r
        .submit_as(&client, &r.unit(json!({"scope":"main","class":"heavy"})))
        .await;
    r.h.queue_job(1, "borg-ci-heavy", 300);
    r.h.queue_job(2, "borg-ci-heavy-deploy", 0);
    r.tick(5).await;
    assert_eq!(
        state_of(&r, unit["unit_id"].as_str().unwrap()).await,
        "queued"
    );
    let leases = r.h.queen.store().active_leases().await.unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(
        leases[0].scope,
        borg::classes::Scope::Deploy,
        "an aged main preallocation must not hide deploy demand before the unit share"
    );
}

async fn submit_at(r: &Rig, client: &str, extra: Value, at: i64) -> String {
    let unit: borg::units::SubmitUnit = serde_json::from_value(r.unit(extra)).unwrap();
    unit.validate().unwrap();
    let id = borg::queen::new_unit_id();
    r.h.queen
        .store()
        .submit_unit(
            &unit,
            client,
            &id,
            3600,
            r.h.queen.config().units.receipt_ttl_secs,
            at,
        )
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn aged_heavy_unit_reserves_before_fresh_work_from_a_less_loaded_client() {
    // Cover both a fresh light unit and an already-aged smaller class.
    for light_created_offset in [119, 0] {
        let r = Rig::new(json!({"starvation_after_secs":30})).await;
        let d = r.drone("d1").await;
        scoped_client(&r, "other", borg::classes::Scope::Pr).await;
        let active = submit_at(
            &r,
            "agent",
            json!({"argv":["active"]}),
            r.base.unix_timestamp(),
        )
        .await;
        r.h.queen
            .store()
            .lease_unit(&active, "d1", r.base.unix_timestamp())
            .await
            .unwrap();
        r.beat(&d, &[&active]).await;
        sustained_ci(&r);
        r.tick(1).await;
        assert_eq!(r.h.queen.store().active_leases().await.unwrap().len(), 7);
        let heavy = submit_at(
            &r,
            "agent",
            json!({"class":"heavy","argv":["aged-heavy"]}),
            r.base.unix_timestamp(),
        )
        .await;
        let fresh = submit_at(
            &r,
            "other",
            json!({"argv":["fresh-light"]}),
            r.base.unix_timestamp() + light_created_offset,
        )
        .await;
        retire_ci(&r, 1, r.base.unix_timestamp() + 119).await;
        r.tick(120).await;
        assert_eq!(state_of(&r, &heavy).await, "queued");
        assert_eq!(
            state_of(&r, &fresh).await,
            "queued",
            "fresh light work cannot consume the last share slot before the aged heavy reservation"
        );
        assert_eq!(
            r.h.queen.store().active_leases().await.unwrap().len(),
            6,
            "continuous CI cannot refill the host reserved for heavy work"
        );
        assert!(
            r.h.status().await["reserved_for"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["class"] == "heavy")
        );
        retire_ci(&r, usize::MAX, r.base.unix_timestamp() + 121).await;
        r.tick(122).await;
        assert_eq!(state_of(&r, &heavy).await, "leased");
        assert_eq!(state_of(&r, &active).await, "running");
        assert_eq!(state_of(&r, &fresh).await, "queued");
    }
}

#[tokio::test]
async fn deploy_prepass_uses_full_uncovered_runner_pressure_before_unit_share() {
    use borg::classes::Scope;
    use borg::explain::{PlacementKind, PlacementOutcome};
    let r = Rig::new(json!({})).await;
    let drone = r.h.enroll("deploy").await;
    r.h.heartbeat_with(&drone, json!({"trust":"deploy","bids":[
        {"class":"heavy","scope":"deploy","available_slots":1,"min_pressure":750,"expires_at":r.base.unix_timestamp()+1000},
        {"class":"heavy","scope":"main","available_slots":1,"expires_at":r.base.unix_timestamp()+1000}
    ]})).await;
    let client = scoped_client(&r, "gate", Scope::Main).await;
    let (_, unit) = r
        .submit_as(&client, &r.unit(json!({"scope":"main","class":"heavy"})))
        .await;
    r.h.queue_job(1, "borg-ci-heavy", 0);
    r.h.queue_job(2, "borg-ci-heavy-deploy", 0);
    r.tick(5).await;
    let leases = r.h.queen.store().active_leases().await.unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].scope, Scope::Deploy);
    assert_eq!(
        state_of(&r, unit["unit_id"].as_str().unwrap()).await,
        "queued",
        "full runner pressure admits Deploy before a Main unit can consume its only heavy slot"
    );
    let history =
        r.h.queen
            .store()
            .placement_history(PlacementKind::Runner, "borg-ci-heavy-deploy", None)
            .await
            .unwrap();
    assert_eq!(history[0].outcome, PlacementOutcome::Granted);
    assert_eq!(history[0].decision.pressure, 1000);
    assert_eq!(history[0].decision.winner.as_deref(), Some("deploy"));
}

#[tokio::test]
async fn pending_pr_runner_covering_main_does_not_inflate_deploy_pressure() {
    use borg::allocator::Rejection;
    use borg::classes::Scope;
    use borg::explain::{PlacementKind, PlacementOutcome};
    let r = Rig::new(json!({})).await;
    r.drone("shared").await;
    r.h.queue_job(1, "borg-ci-heavy-pr", 0);
    r.tick(0).await;
    let pending = r.h.queen.store().active_leases().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].scope, Scope::Pr);
    assert_eq!(pending[0].state, borg::store::LeaseStatus::Pending);

    let drone = r.h.enroll("deploy").await;
    r.h.heartbeat_with(&drone, json!({"trust":"deploy","bids":[
        {"class":"heavy","scope":"deploy","available_slots":1,"min_pressure":750,"expires_at":r.base.unix_timestamp()+1000},
        {"class":"heavy","scope":"main","available_slots":1,"expires_at":r.base.unix_timestamp()+1000}
    ]})).await;
    let client = scoped_client(&r, "gate", Scope::Main).await;
    let (_, unit) = r
        .submit_as(&client, &r.unit(json!({"scope":"main","class":"heavy"})))
        .await;
    r.h.state().jobs.clear();
    r.h.queue_job(2, "borg-ci-heavy", 0);
    r.h.queue_job(3, "borg-ci-heavy-deploy", 0);
    r.tick(5).await;
    let leases = r.h.queen.store().active_leases().await.unwrap();
    assert_eq!(
        leases.len(),
        1,
        "the existing pending PR runner covers Main CI"
    );
    assert_eq!(leases[0].id, pending[0].id);
    assert_eq!(
        state_of(&r, unit["unit_id"].as_str().unwrap()).await,
        "leased"
    );
    let history =
        r.h.queen
            .store()
            .placement_history(PlacementKind::Runner, "borg-ci-heavy-deploy", None)
            .await
            .unwrap();
    assert!(
        history
            .iter()
            .all(|row| row.outcome == PlacementOutcome::Unplaced)
    );
    assert!(history.iter().any(|row| row.decision.pressure == 500
        && row.decision.candidates.iter().any(|candidate| {
            candidate.drone_id == "deploy"
                && candidate
                    .rejection_reasons
                    .contains(&Rejection::BidThreshold)
        })));
}

#[tokio::test]
async fn disabled_units_show_only_that_they_are_off() {
    let h = support::Harness::start(json!({})).await;
    let s = h.status().await;
    assert_eq!(s["units"]["enabled"], false);
    assert_eq!(s["units"]["counts"], json!({}));
    assert_eq!(s["units"]["recent"], json!([]));
    assert!(!h.metrics().await.contains("borg_units{"));
}
