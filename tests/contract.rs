mod support;

use borg::enroll;
use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

fn good(toolchain: &str) -> Value {
    json!({
        "observed_at": now(),
        "rustc_wrapper": "/usr/local/bin/sccache", "sccache_dir": "/var/cache/sccache",
        "sccache_server_reachable": true, "sccache_stats_ok": true,
        "cache_requests": 1000, "cache_hits": 500, "cache_errors": 0,
        "cache_dir_free_bytes": 100u64 << 30, "cache_write_ok": true,
        "toolchains": [{"channel": toolchain, "rustc_version": toolchain}],
        "mem_available_mib": 30_000, "cpu_psi_some_avg10_pm": 50
    })
}

fn cfg(extra: Value) -> Value {
    let mut c = json!({
        "drone_stale_secs": 1_000_000,
        "env_contract": {
            "enabled": true,
            "checks": {"compiler_cache": "required", "toolchain": "required",
                       "cache_store": "required", "resources": "required"}
        },
        "units": {"enabled": true}
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        c[k] = v;
    }
    c
}

async fn beat(h: &Harness, token: &str, facts: Option<Value>) -> Value {
    match facts {
        Some(f) => h.heartbeat_with(token, json!({"env_facts": f})).await,
        None => h.heartbeat_with(token, json!({})).await,
    }
}

fn drone<'a>(status: &'a Value, name: &str) -> &'a Value {
    status["drones"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == name)
        .unwrap()
}

#[tokio::test]
async fn a_failing_required_check_keeps_ci_leases_off_that_drone() {
    let h = Harness::start(cfg(json!({}))).await;
    // "a-bad" would win every name tie; it has no compiler cache wrapper.
    let bad = h.enroll("a-bad").await;
    let ok = h.enroll("b-good").await;
    let mut broken = good("1.95.0");
    broken["rustc_wrapper"] = Value::Null;
    beat(&h, &bad, Some(broken)).await;
    beat(&h, &ok, Some(good("1.95.0"))).await;

    h.queue_job(1, "borg-ci-light-pr", 5);
    h.tick().await;
    assert!(
        beat(&h, &bad, None).await["leases"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        beat(&h, &ok, None).await["leases"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let s = h.status().await;
    let c = &drone(&s, "a-bad")["contract"];
    assert_eq!(c["admission"]["light"]["admit"], false);
    assert!(
        c["admission"]["light"]["why"]
            .as_str()
            .unwrap()
            .contains("RUSTC_WRAPPER is not set")
    );
    let cc = c["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["kind"] == "compiler_cache")
        .unwrap();
    assert_eq!(
        (cc["state"].as_str(), cc["mode"].as_str()),
        (Some("fail"), Some("required"))
    );
    assert_eq!(
        drone(&s, "b-good")["contract"]["admission"]["heavy"]["admit"],
        true
    );
    // A blocked drone shows no free slots for the class it is not admitted to.
    assert_eq!(drone(&s, "a-bad")["free_slots"]["light"], 0);

    let m = h.metrics().await;
    for line in [
        "borg_env_contract_check{drone=\"a-bad\",check=\"compiler_cache\",state=\"fail\"} 1",
        "borg_env_contract_check{drone=\"b-good\",check=\"compiler_cache\",state=\"pass\"} 1",
        "borg_env_contract_admitted{drone=\"a-bad\",class=\"light\"} 0",
        "borg_env_contract_admitted{drone=\"b-good\",class=\"light\"} 1",
    ] {
        assert!(m.contains(line), "missing {line}\n{m}");
    }
}

#[tokio::test]
async fn units_are_placed_only_on_drones_with_the_toolchain_the_tree_asks_for() {
    let h = Harness::start(cfg(json!({}))).await;
    let client = enroll::new_token();
    h.queen
        .store()
        .enroll_client("agent", &enroll::hash_token(&client), 0)
        .await
        .unwrap();
    let old = h.enroll("a-old").await;
    let new = h.enroll("b-new").await;
    beat(&h, &old, Some(good("1.90.0"))).await;
    beat(&h, &new, Some(good("1.95.0"))).await;

    let r = h
        .http
        .post(format!("{}/v1/units", h.url))
        .bearer_auth(&client)
        .json(&json!({"repo": "example-org/app", "tree_sha": "a".repeat(40), "argv": ["cargo", "check"],
                      "platform": "linux-x86_64", "class": "light", "rust_toolchain": "1.95.0"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    h.tick().await;
    assert!(
        beat(&h, &old, None).await["units"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        beat(&h, &new, None).await["units"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Nobody has 1.99.0: the unit waits, and the status says why for each drone.
    let r = h
        .http
        .post(format!("{}/v1/units", h.url))
        .bearer_auth(&client)
        .json(&json!({"repo": "example-org/app", "tree_sha": "b".repeat(40), "argv": ["cargo", "check"],
                      "platform": "linux-x86_64", "class": "light", "rust_toolchain": "1.99.0"}))
        .send()
        .await
        .unwrap();
    let id = r.json::<Value>().await.unwrap()["unit_id"]
        .as_str()
        .unwrap()
        .to_owned();
    h.tick().await;
    assert_eq!(
        h.queen
            .store()
            .unit(&id)
            .await
            .unwrap()
            .unwrap()
            .state
            .as_str(),
        "queued"
    );
    let status = h.status().await;
    let blocks = status["units"]["not_admitted"].as_array().unwrap();
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert!(blocks.iter().all(|b| b["unit_id"] == id.as_str()));
    assert!(
        blocks[0]["why"]
            .as_str()
            .unwrap()
            .contains("toolchain 1.99.0 is not installed (have 1.90.0)")
    );
}

#[tokio::test]
async fn memory_and_cpu_pressure_are_checked_against_the_class_needs() {
    let h = Harness::start(cfg(json!({}))).await;
    let t = h.enroll("d1").await;
    let mut f = good("1.95.0");
    f["mem_available_mib"] = json!(10_000); // heavy needs 24 GiB, medium 12 GiB
    beat(&h, &t, Some(f)).await;
    h.tick().await;
    let a = &drone(&h.status().await, "d1")["contract"]["admission"].clone();
    assert_eq!(
        (
            a["light"]["admit"].as_bool(),
            a["medium"]["admit"].as_bool(),
            a["heavy"]["admit"].as_bool()
        ),
        (Some(true), Some(false), Some(false))
    );
    assert!(
        a["heavy"]["why"]
            .as_str()
            .unwrap()
            .contains("10000 MiB available")
    );

    // CPU pressure over the ceiling blocks everything.
    let mut f = good("1.95.0");
    f["cpu_psi_some_avg10_pm"] = json!(950);
    beat(&h, &t, Some(f)).await;
    let a = &drone(&h.status().await, "d1")["contract"]["admission"].clone();
    assert_eq!(a["light"]["admit"], false);
}

#[tokio::test]
async fn drones_that_never_reported_are_admitted_by_default_and_stale_facts_are_unknown() {
    let h = Harness::start(cfg(json!({}))).await;
    let t = h.enroll("old-drone").await;
    h.heartbeat(&t, &[]).await;
    h.queue_job(1, "borg-ci-light-pr", 5);
    h.tick().await;
    // Predates the contract: still gets work.
    assert_eq!(
        beat(&h, &t, None).await["leases"].as_array().unwrap().len(),
        1
    );
    let s = h.status().await;
    assert_eq!(drone(&s, "old-drone")["contract"]["reported"], false);

    // Facts that go stale are unknown, which a required check treats as blocking.
    let t2 = h.enroll("z-drone").await;
    let mut f = good("1.95.0");
    f["observed_at"] = json!(now() - 3600);
    beat(&h, &t2, Some(f)).await;
    let s = h.status().await;
    let c = &drone(&s, "z-drone")["contract"];
    assert_eq!(c["admission"]["light"]["admit"], false);
    assert_eq!(c["checks"][0]["state"], "unknown");
}

#[tokio::test]
async fn the_contract_is_off_by_default_and_require_report_is_opt_in() {
    // Off: even terrible facts block nothing and no contract appears.
    let h = Harness::start(json!({"drone_stale_secs": 1_000_000})).await;
    let t = h.enroll("d1").await;
    let mut f = good("1.95.0");
    f["rustc_wrapper"] = Value::Null;
    f["mem_available_mib"] = json!(1);
    beat(&h, &t, Some(f)).await;
    h.queue_job(1, "borg-ci-light-pr", 5);
    h.tick().await;
    assert_eq!(
        beat(&h, &t, None).await["leases"].as_array().unwrap().len(),
        1
    );
    assert!(drone(&h.status().await, "d1")["contract"].is_null());

    // require_report: a drone with no facts gets nothing.
    let h = Harness::start(cfg(
        json!({"env_contract": {"enabled": true, "require_report": true}}),
    ))
    .await;
    let t = h.enroll("d1").await;
    h.heartbeat(&t, &[]).await;
    h.queue_job(1, "borg-ci-light-pr", 5);
    h.tick().await;
    assert!(
        beat(&h, &t, None).await["leases"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_drone_missing_a_required_tool_gets_no_lease_and_status_names_it() {
    let h = Harness::start(cfg(json!({
        "env_contract": {
            "enabled": true,
            "checks": {"tools": "required"},
            "required_tools": ["git", "gh", "node"],
            "classes": {"heavy": {"required_tools": ["git"]}}
        }
    })))
    .await;
    // "a-bare" would win every name tie; it came up without gh and node.
    let bare = h.enroll("a-bare").await;
    let full = h.enroll("b-full").await;
    let mut f = good("1.95.0");
    f["tools"] =
        json!({"git": "git version 2.45.0", "gh": "missing", "node": "missing", "jq": "jq-1.7"});
    beat(&h, &bare, Some(f)).await;
    let mut f = good("1.95.0");
    f["tools"] = json!({"git": "git version 2.45.0", "gh": "gh version 2.50.0", "node": "v22.1.0"});
    beat(&h, &full, Some(f)).await;

    h.queue_job(1, "borg-ci-light-pr", 5);
    h.tick().await;
    assert!(
        beat(&h, &bare, None).await["leases"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        beat(&h, &full, None).await["leases"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let s = h.status().await;
    let c = &drone(&s, "a-bare")["contract"];
    assert_eq!(c["admission"]["light"]["admit"], false);
    assert!(
        c["admission"]["light"]["why"]
            .as_str()
            .unwrap()
            .contains("required tools missing on PATH: gh, node")
    );
    // The heavy class only asks for git.
    assert_eq!(c["admission"]["heavy"]["admit"], true);
    let find_tools = |c: &Value| {
        c["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["kind"] == "tools")
            .unwrap()
            .clone()
    };
    let t = find_tools(c);
    assert_eq!(
        (t["state"].as_str(), t["mode"].as_str()),
        (Some("fail"), Some("required"))
    );
    assert_eq!(t["observed"]["missing"], "gh,node");
    assert!(
        t["observed"]["versions"]
            .as_str()
            .unwrap()
            .contains("git=git version 2.45.0")
    );
    assert_eq!(
        find_tools(&drone(&s, "b-full")["contract"])["state"],
        "pass"
    );

    let m = h.metrics().await;
    for line in [
        "borg_env_contract_check{drone=\"a-bare\",check=\"tools\",state=\"fail\"} 1",
        "borg_env_contract_check{drone=\"b-full\",check=\"tools\",state=\"pass\"} 1",
    ] {
        assert!(m.contains(line), "missing {line}\n{m}");
    }
}
