use crate::support;

use borg::enroll;
use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

const GIB: u64 = 1 << 30;

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

fn scoped_disk_cfg(extra: Value) -> Value {
    let mut merged = json!({
        "env_contract": {
            "classes": {
                "heavy": {"min_cache_dir_free_bytes": 32u64 << 30}
            }
        }
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        merged[k] = v;
    }
    cfg(merged)
}

fn facts_with_cache_free(cache_free_bytes: u64) -> Value {
    let mut facts = good("1.95.0");
    facts["cache_dir_free_bytes"] = json!(cache_free_bytes);
    facts
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

#[tokio::test]
async fn class_specific_job_user_proofs_gate_ci_and_survive_heartbeat_storage() {
    let h = Harness::start(cfg(json!({
        "env_contract": {
            "enabled": true,
            "checks": {"capabilities": "required"},
            "classes": {"heavy": {"required_capabilities": ["sandbox", "docker"]}}
        }
    })))
    .await;
    let old = h.enroll("a-inventory-only").await;
    let ok = h.enroll("b-proven").await;
    let stale = h.enroll("c-stale-proof").await;
    let mut fresh = good("1.95.0");
    fresh["capabilities"] = json!({
        "sandbox": {"observed_at": now(), "state": "pass"},
        "docker": {"observed_at": now(), "state": "pass"}
    });
    beat(&h, &old, Some(good("1.95.0"))).await;
    beat(&h, &ok, Some(fresh.clone())).await;
    fresh["capabilities"]["sandbox"]["observed_at"] = json!(now() - 3600);
    beat(&h, &stale, Some(fresh)).await;
    h.queue_job(1, "borg-ci-heavy-pr", 5);
    h.tick().await;
    for token in [&old, &stale] {
        assert!(
            beat(&h, token, None).await["leases"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        beat(&h, &ok, None).await["leases"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let status = h.status().await;
    for name in ["a-inventory-only", "c-stale-proof"] {
        let admission = &drone(&status, name)["contract"]["admission"];
        assert_eq!(admission["heavy"]["admit"], false);
        assert_eq!(admission["light"]["admit"], true);
        assert!(
            admission["heavy"]["why"]
                .as_str()
                .unwrap()
                .contains("sandbox")
        );
    }
    let stored = h.queen.store().drones().await.unwrap();
    let stale = stored.iter().find(|d| d.name == "c-stale-proof").unwrap();
    assert!(
        stale.env_facts.as_ref().unwrap().capabilities[&borg::envcontract::Capability::Sandbox]
            .observed_at
            < now() - 3500
    );
}

#[tokio::test]
async fn disk_floor_blocks_new_delivery_and_placement_with_contract_off_but_keeps_active_work() {
    let h = Harness::start(
        json!({"env_contract": {"enabled": false, "min_cache_dir_free_bytes": 32u64 << 30}}),
    )
    .await;
    let token = h.enroll("disk").await;
    h.heartbeat_with(&token, json!({})).await;
    h.queue_job(101, "borg-ci-light-pr", 1);
    h.tick().await;
    let initial = h.heartbeat_with(&token, json!({})).await;
    assert_eq!(initial["min_free_disk_bytes"], 32u64 << 30);
    let active = initial["leases"][0]["lease_id"].as_str().unwrap();
    let mut low = serde_json::to_value(support::capacity()).unwrap();
    low["free_disk_mib"] = json!(8192);
    let held = h
        .heartbeat_with(&token, json!({"capacity": low, "active_leases": [active]}))
        .await;
    assert!(held["leases"].as_array().unwrap().is_empty());
    assert!(held["cancel_leases"].as_array().unwrap().is_empty());
    h.queue_job(102, "borg-ci-light-pr", 2);
    h.tick().await;
    assert_eq!(h.state().mints.len(), 1, "no new lease under the floor");
    let status = h.status().await;
    let d = drone(&status, "disk");
    assert_eq!(d["accepting"], false);
    assert_eq!(d["active_leases"], 1);
    assert_eq!(d["disk_admission"]["state"], "fail");
    assert!(
        d["contract"].is_null(),
        "safety does not enable the optional contract"
    );
    h.heartbeat_with(&token, json!({"active_leases": [active]}))
        .await;
    h.tick().await;
    assert_eq!(h.state().mints.len(), 2, "recovery restores admission");
}

/// A heartbeat capacity with `free_gib` free on the work filesystem.
fn capacity_with_free(free_gib: u64) -> Value {
    let mut capacity = serde_json::to_value(support::capacity()).unwrap();
    capacity["free_disk_mib"] = json!(free_gib * 1024);
    capacity
}

/// Four light jobs start on one drone with 60 GiB free and write half of it;
/// two more are then queued. Returns the runners minted in total.
async fn mints_beside_four_running_jobs(config: Value) -> usize {
    let h = Harness::start(config).await;
    let token = h.enroll("shared").await;
    h.heartbeat_with(&token, json!({"capacity": capacity_with_free(60)}))
        .await;
    for id in 0..4 {
        h.queue_job(600 + id, "borg-ci-light-pr", 1);
    }
    h.tick().await;
    assert_eq!(h.state().mints.len(), 4, "four 10 GiB floors fit 60 GiB");
    h.set_runner("borg-shared-", "online", true);
    h.state().jobs.clear();
    h.tick().await;
    assert_eq!(h.status().await["classes"]["light"]["leases_running"], 4);

    h.heartbeat_with(&token, json!({"capacity": capacity_with_free(30)}))
        .await;
    h.queue_job(610, "borg-ci-light-pr", 1);
    h.queue_job(611, "borg-ci-light-pr", 1);
    h.tick().await;
    let minted = h.state().mints.len();

    // Once the running jobs end, the same reading admits the waiting job.
    h.state().runners.retain(|r| r["busy"] != json!(true));
    h.tick().await;
    h.tick().await;
    assert_eq!(
        h.state().mints.len(),
        6,
        "finished jobs release their reserve"
    );
    minted
}

#[tokio::test]
async fn running_jobs_keep_a_disk_reserve_so_new_starts_do_not_fill_a_shared_disk() {
    // By default each running lease keeps 4 GiB: 30 GiB less 16 holds one
    // more 10 GiB floor, not the three the reading alone would promise.
    assert_eq!(mints_beside_four_running_jobs(json!({})).await, 5);
    // A class's own value replaces the global one.
    assert_eq!(
        mints_beside_four_running_jobs(json!({"env_contract": {
            "classes": {"light": {"running_reserve_bytes": 6u64 << 30}}
        }}))
        .await,
        4
    );
    // Reserving nothing is the old rule: the reading alone admits both.
    assert_eq!(
        mints_beside_four_running_jobs(json!({"env_contract": {"running_reserve_bytes": 0}})).await,
        6
    );
}

#[tokio::test]
async fn a_heartbeat_and_status_judge_disk_with_the_reserves_of_running_jobs() {
    let h = Harness::start(json!({})).await;
    let token = h.enroll("shared").await;
    let beat = |free_gib: u64, active: Vec<String>| json!({"capacity": capacity_with_free(free_gib), "active_leases": active});
    h.heartbeat_with(&token, beat(60, vec![])).await;
    for id in 0..4 {
        h.queue_job(620 + id, "borg-ci-light-pr", 1);
    }
    h.tick().await;
    let handed = h.heartbeat_with(&token, beat(60, vec![])).await;
    let running: Vec<String> = handed["leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["lease_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(running.len(), 4);
    h.set_runner("borg-shared-", "online", true);
    h.state().jobs.clear();
    h.heartbeat_with(&token, beat(30, running.clone())).await;
    h.tick().await;

    // `/status` counts the room placement would use: memory and vCPU hold
    // four more light leases, the disk one (30 GiB less 16 reserved).
    let status = h.status().await;
    assert_eq!(drone(&status, "shared")["free_slots"]["light"], 1);
    assert_eq!(status["classes"]["light"]["free_slots"], 1);

    // A fifth runner is minted against 30 GiB. Before it is handed over the
    // reading falls to 15: under the 16 GiB the running jobs keep, so the
    // heartbeat keeps it back though 15 clears the 10 GiB floor alone.
    h.queue_job(630, "borg-ci-light-pr", 1);
    h.tick().await;
    assert_eq!(h.state().mints.len(), 5);
    let low = h.heartbeat_with(&token, beat(15, running.clone())).await;
    assert!(low["leases"].as_array().unwrap().is_empty(), "{low}");
    assert!(low["cancel_leases"].as_array().unwrap().is_empty());

    // The runner kept back covers no demand: the job goes to another drone.
    let other = h.enroll("other").await;
    h.heartbeat_with(&other, json!({})).await;
    h.tick().await;
    let mints = h.state().mints.clone();
    assert_eq!(mints.len(), 6, "the waiting job is placed elsewhere");
    assert!(
        mints[5]["name"]
            .as_str()
            .unwrap()
            .starts_with("borg-other-"),
        "{}",
        mints[5]
    );

    // With the reading back, the same runner is handed over.
    let back = h.heartbeat_with(&token, beat(30, running)).await;
    assert_eq!(back["leases"].as_array().unwrap().len(), 1, "{back}");
}

#[tokio::test]
async fn reclaim_floor_is_the_disk_floor_of_the_largest_class_the_drone_hosts() {
    let h = Harness::start(json!({"env_contract": {
        "enabled": false,
        "classes": {"heavy": {"min_cache_dir_free_bytes": 32u64 << 30}}
    }}))
    .await;
    let token = h.enroll("disk").await;
    let heavy_host = h.heartbeat_with(&token, json!({})).await;
    assert_eq!(heavy_host["min_free_disk_bytes"], 10u64 << 30);
    assert_eq!(heavy_host["reclaim_floor_bytes"], 32u64 << 30);

    let mut small = serde_json::to_value(support::capacity()).unwrap();
    small["mem_mib"] = json!(6 * 1024);
    let light_host = h.heartbeat_with(&token, json!({"capacity": small})).await;
    assert_eq!(
        light_host["reclaim_floor_bytes"],
        10u64 << 30,
        "a host too small for a heavy lease keeps the admission floor"
    );

    // A drone that takes leases but cannot right now (under the disk floor, a failed
    // invariant, draining) still hosts heavy leases, and needs the heavy floor to recover.
    let blocked = h
        .heartbeat_with(&token, json!({"accepting": false, "takes_leases": true}))
        .await;
    assert_eq!(
        blocked["reclaim_floor_bytes"],
        32u64 << 30,
        "a blocked heavy host keeps the heavy floor: {blocked}"
    );
    // So does a host whose Docker daemon is down: the drone reports the invariant failure, and
    // a daemon outage does not turn a heavy host into one that hosts no Docker class.
    let mut no_docker = serde_json::to_value(support::capacity()).unwrap();
    no_docker["docker_usable"] = json!(false);
    let daemon_down = h
        .heartbeat_with(
            &token,
            json!({
                "capacity": no_docker,
                "accepting": false,
                "takes_leases": true,
                "env_facts": {"observed_at": 0, "host_invariant_failures": [
                    {"check": "docker_daemon", "detail": "the daemon does not answer"}
                ]},
            }),
        )
        .await;
    assert_eq!(
        daemon_down["reclaim_floor_bytes"],
        32u64 << 30,
        "a Docker outage keeps the heavy floor: {daemon_down}"
    );
    // A host with no Docker at all hosts no Docker class and keeps the admission floor.
    let without_docker = h
        .heartbeat_with(&token, json!({"capacity": no_docker, "takes_leases": true}))
        .await;
    assert_eq!(without_docker["reclaim_floor_bytes"], 10u64 << 30);
    for reclaim_only in [
        json!({"accepting": false, "takes_leases": false}),
        // A drone from before `takes_leases`: the queen can only read `accepting`.
        json!({"accepting": false}),
    ] {
        let response = h.heartbeat_with(&token, reclaim_only).await;
        assert!(
            response.get("reclaim_floor_bytes").is_none(),
            "a drone that takes no leases keeps its own floor: {response}"
        );
    }
}

#[tokio::test]
async fn low_cache_blocks_pending_delivery_without_cache_stats_and_recovers_at_exact_floor() {
    let h = Harness::start(json!({"env_contract": {"checks": {"cache_store": "advisory"}}})).await;
    let token = h.enroll("disk").await;
    h.heartbeat_with(&token, json!({})).await;
    h.queue_job(201, "borg-ci-light-pr", 1);
    h.tick().await;
    let low = h
        .heartbeat_with(
            &token,
            json!({"env_facts": {"observed_at": now(), "cache_dir_free_bytes": 1}}),
        )
        .await;
    assert!(
        low["leases"].as_array().unwrap().is_empty(),
        "pending JIT remains unstarted"
    );
    let leases = h.queen.store().active_leases().await.unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].state, borg::store::LeaseStatus::Pending);
    let recovered = h
        .heartbeat_with(
            &token,
            json!({"env_facts": {"observed_at": now(), "cache_dir_free_bytes": 10u64 << 30}}),
        )
        .await;
    assert_eq!(recovered["leases"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn stale_cache_space_is_unknown_and_does_not_override_current_work_capacity() {
    let h = Harness::start(json!({"env_contract": {"facts_max_age_secs": 2}})).await;
    let token = h.enroll("disk").await;
    h.heartbeat_with(
        &token,
        json!({"env_facts": {"observed_at": now() - 10, "cache_dir_free_bytes": 0}}),
    )
    .await;
    h.queue_job(301, "borg-ci-light-pr", 1);
    h.tick().await;
    assert_eq!(h.state().mints.len(), 1);
    let status = h.status().await;
    let d = drone(&status, "disk");
    assert_eq!(d["accepting"], true);
    assert_eq!(d["disk_admission"]["state"], "unknown");
    assert!(d["disk_admission"]["cache_free_bytes"].is_null());
}

#[tokio::test]
async fn scoped_disk_floor_blocks_heavy_only_and_recovers_at_exact_floor() {
    let h = Harness::start(scoped_disk_cfg(json!({}))).await;
    let at_20_4 = h.enroll("disk-20").await;
    let at_30_1 = h.enroll("disk-30").await;
    h.heartbeat_with(
        &at_20_4,
        json!({"env_facts": facts_with_cache_free(20 * GIB + 2 * GIB / 5)}),
    )
    .await;
    h.heartbeat_with(
        &at_30_1,
        json!({"env_facts": facts_with_cache_free(30 * GIB + GIB / 10)}),
    )
    .await;

    let status = h.status().await;
    for name in ["disk-20", "disk-30"] {
        let d = drone(&status, name);
        assert_eq!(d["accepting"], true);
        assert_eq!(d["disk_admission"]["minimum_free_bytes"], 10 * GIB);
        assert_eq!(
            d["class_disk_admission"]["light"]["minimum_free_bytes"],
            10 * GIB
        );
        assert_eq!(
            d["class_disk_admission"]["medium"]["minimum_free_bytes"],
            10 * GIB
        );
        assert_eq!(d["class_disk_admission"]["medium"]["state"], "pass");
        assert_eq!(
            d["class_disk_admission"]["heavy"]["minimum_free_bytes"],
            32 * GIB
        );
        assert_eq!(d["class_disk_admission"]["heavy"]["state"], "fail");
        assert!(
            d["class_disk_admission"]["heavy"]["reason"]
                .as_str()
                .unwrap()
                .contains("cache directory")
        );
        assert_eq!(d["free_slots"]["heavy"], 0);
        assert_eq!(d["theoretical_slots"]["heavy"], 0);
        assert!(d["free_slots"]["light"].as_u64().unwrap() > 0);
        assert!(d["theoretical_slots"]["light"].as_u64().unwrap() > 0);
        assert!(
            d["contract"].is_null(),
            "the optional contract stays disabled"
        );
    }

    h.queue_job(400, "borg-ci-light-pr", 1);
    h.queue_job(401, "borg-ci-heavy-pr", 1);
    h.tick().await;
    let mints = h.state().mints.clone();
    assert_eq!(mints.len(), 1);
    assert!(
        mints[0]["labels"]
            .as_array()
            .unwrap()
            .contains(&json!("borg-ci-light-pr")),
        "light remains placeable while heavy is blocked"
    );
    assert!(mints.iter().all(|mint| {
        !mint["labels"]
            .as_array()
            .unwrap()
            .contains(&json!("borg-ci-heavy-pr"))
    }));

    h.heartbeat_with(
        &at_20_4,
        json!({"env_facts": facts_with_cache_free(32 * GIB)}),
    )
    .await;
    h.tick().await;
    assert_eq!(
        h.state().mints.len(),
        2,
        "exactly the configured floor admits heavy"
    );
    assert!(h.state().mints.iter().any(|mint| {
        mint["labels"]
            .as_array()
            .unwrap()
            .contains(&json!("borg-ci-heavy-pr"))
    }));
}

#[tokio::test]
async fn scoped_disk_floor_rechecks_pending_lease_and_never_cancels_active_work() {
    let h = Harness::start(scoped_disk_cfg(json!({}))).await;
    let token = h.enroll("disk").await;
    h.heartbeat_with(
        &token,
        json!({"env_facts": facts_with_cache_free(32 * GIB)}),
    )
    .await;
    h.queue_job(501, "borg-ci-heavy-pr", 1);
    h.tick().await;
    assert_eq!(h.state().mints.len(), 1);

    let blocked = h
        .heartbeat_with(
            &token,
            json!({"env_facts": facts_with_cache_free(30 * GIB + GIB / 10)}),
        )
        .await;
    assert!(blocked["leases"].as_array().unwrap().is_empty());
    assert!(blocked["cancel_leases"].as_array().unwrap().is_empty());
    let omitted = h.heartbeat_with(&token, json!({})).await;
    assert!(
        omitted["leases"].as_array().unwrap().is_empty(),
        "a heartbeat without new facts still uses the latest fresh stored cache measurement"
    );
    let pending = h.queen.store().active_leases().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].state, borg::store::LeaseStatus::Pending);

    let recovered = h
        .heartbeat_with(
            &token,
            json!({"env_facts": facts_with_cache_free(32 * GIB)}),
        )
        .await;
    let id = recovered["leases"][0]["lease_id"].as_str().unwrap();
    let active = h
        .heartbeat_with(
            &token,
            json!({
                "active_leases": [id],
                "env_facts": facts_with_cache_free(20 * GIB + 2 * GIB / 5)
            }),
        )
        .await;
    assert!(active["cancel_leases"].as_array().unwrap().is_empty());
    let status = h.status().await;
    assert_eq!(drone(&status, "disk")["active_leases"], 1);
}

#[tokio::test]
async fn scoped_disk_floor_rechecks_standby_delivery_and_start() {
    let h = Harness::start(scoped_disk_cfg(json!({
        "standby": {"classes": ["heavy"], "per_drone": 1}
    })))
    .await;
    let token = h.enroll("deploy").await;
    h.heartbeat_with(
        &token,
        json!({"trust": "deploy-only", "env_facts": facts_with_cache_free(32 * GIB)}),
    )
    .await;
    h.tick().await;

    let blocked = h
        .heartbeat_with(
            &token,
            json!({"trust": "deploy-only", "env_facts": facts_with_cache_free(30 * GIB + GIB / 10)}),
        )
        .await;
    assert!(blocked["standby"].as_array().unwrap().is_empty());
    let recovered = h
        .heartbeat_with(
            &token,
            json!({"trust": "deploy-only", "env_facts": facts_with_cache_free(32 * GIB)}),
        )
        .await;
    let id = recovered["standby"][0]["lease_id"]
        .as_str()
        .unwrap()
        .to_owned();

    h.queue_job(601, "borg-ci-heavy-deploy", 1);
    h.tick().await;
    let blocked_start = h
        .heartbeat_with(
            &token,
            json!({
                "trust": "deploy-only",
                "held_standby": [&id],
                "env_facts": facts_with_cache_free(30 * GIB + GIB / 10)
            }),
        )
        .await;
    assert!(
        blocked_start["start_standby"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let recovered_start = h
        .heartbeat_with(
            &token,
            json!({
                "trust": "deploy-only",
                "held_standby": [&id],
                "env_facts": facts_with_cache_free(32 * GIB)
            }),
        )
        .await;
    assert_eq!(recovered_start["start_standby"], json!([id]));
}

#[tokio::test]
async fn scoped_disk_floor_rechecks_unit_placement_delivery_and_active_retention() {
    let h = Harness::start(scoped_disk_cfg(json!({}))).await;
    let client = enroll::new_token();
    h.queen
        .store()
        .enroll_client("agent", &enroll::hash_token(&client), 0)
        .await
        .unwrap();
    let token = h.enroll("unit-host").await;
    h.heartbeat_with(
        &token,
        json!({"env_facts": facts_with_cache_free(20 * GIB + 2 * GIB / 5)}),
    )
    .await;

    let response = h
        .http
        .post(format!("{}/v1/units", h.url))
        .bearer_auth(&client)
        .json(&json!({
            "repo": "example-org/app",
            "tree_sha": "c".repeat(40),
            "argv": ["cargo", "check"],
            "platform": "linux-x86_64",
            "class": "heavy",
            "rust_toolchain": "1.95.0"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let id = response.json::<Value>().await.unwrap()["unit_id"]
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
        "queued",
        "the unit-group recomputation preserves the class disk block"
    );

    h.heartbeat_with(
        &token,
        json!({"env_facts": facts_with_cache_free(32 * GIB)}),
    )
    .await;
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
        "leased"
    );
    let blocked = h
        .heartbeat_with(
            &token,
            json!({"env_facts": facts_with_cache_free(20 * GIB + 2 * GIB / 5)}),
        )
        .await;
    assert!(blocked["units"].as_array().unwrap().is_empty());

    let recovered = h
        .heartbeat_with(
            &token,
            json!({"env_facts": facts_with_cache_free(32 * GIB)}),
        )
        .await;
    assert_eq!(recovered["units"][0]["unit_id"], id);
    let active = h
        .heartbeat_with(
            &token,
            json!({
                "active_units": [&id],
                "env_facts": facts_with_cache_free(20 * GIB + 2 * GIB / 5)
            }),
        )
        .await;
    assert!(active["cancel_units"].as_array().unwrap().is_empty());
    assert_eq!(
        h.queen
            .store()
            .unit(&id)
            .await
            .unwrap()
            .unwrap()
            .state
            .as_str(),
        "running"
    );
}

#[tokio::test]
async fn scoped_disk_floor_keeps_stale_and_missing_measurements_compatible() {
    let h = Harness::start(scoped_disk_cfg(json!({}))).await;
    let missing = h.enroll("missing").await;
    let stale = h.enroll("stale").await;
    let mut no_cache = facts_with_cache_free(0);
    no_cache
        .as_object_mut()
        .unwrap()
        .remove("cache_dir_free_bytes");
    h.heartbeat_with(&missing, json!({"env_facts": no_cache}))
        .await;
    let mut old = facts_with_cache_free(0);
    old["observed_at"] = json!(now() - 1_000);
    h.heartbeat_with(&stale, json!({"env_facts": old})).await;

    let status = h.status().await;
    for name in ["missing", "stale"] {
        let d = drone(&status, name);
        assert_eq!(d["class_disk_admission"]["heavy"]["state"], "unknown");
        assert_eq!(
            d["class_disk_admission"]["heavy"]["minimum_free_bytes"],
            32 * GIB
        );
        assert!(d["free_slots"]["heavy"].as_u64().unwrap() > 0);
    }
}

#[tokio::test]
async fn scoped_disk_floor_leaves_blocked_drones_out_of_the_fleet_class_slots() {
    let h = Harness::start(scoped_disk_cfg(json!({}))).await;
    let token = h.enroll("disk").await;
    h.heartbeat_with(
        &token,
        json!({"env_facts": facts_with_cache_free(20 * GIB)}),
    )
    .await;
    h.tick().await;
    let status = h.status().await;
    assert_eq!(drone(&status, "disk")["theoretical_slots"]["heavy"], 0);
    assert_eq!(status["classes"]["heavy"]["slots"], 0, "{status}");
    assert!(status["classes"]["light"]["slots"].as_u64().unwrap() > 0);
    let m = h.metrics().await;
    assert!(
        m.contains("borg_class_slots{class=\"heavy\"} 0"),
        "a class every drone blocks has no fleet slots\n{m}"
    );

    // Exactly the floor admits heavy again.
    h.heartbeat_with(
        &token,
        json!({"env_facts": facts_with_cache_free(32 * GIB)}),
    )
    .await;
    h.tick().await;
    let status = h.status().await;
    assert_eq!(status["classes"]["heavy"]["slots"], 1, "{status}");
    assert!(
        h.metrics()
            .await
            .contains("borg_class_slots{class=\"heavy\"} 1")
    );
}

async fn standby_on(h: &Harness, name: &str) -> Vec<String> {
    h.queen
        .store()
        .active_leases()
        .await
        .unwrap()
        .into_iter()
        .filter(|l| l.drone == name && l.state == borg::store::LeaseStatus::Standby)
        .map(|l| l.id)
        .collect()
}

#[tokio::test]
async fn a_standby_below_its_class_floor_neither_covers_deploy_demand_nor_is_started() {
    let deploy =
        |free: u64| json!({"trust": "deploy-only", "env_facts": facts_with_cache_free(free)});
    // Each pass runs ten seconds after the one before and never behind the
    // wall clock. Jobs and heartbeats are stamped with the wall clock, so a
    // pass timed from the start of the test would, on a slow host, run before
    // the job it should place was queued and place nothing.
    let after = |earlier: OffsetDateTime| {
        (earlier + time::Duration::seconds(10)).max(OffsetDateTime::now_utc())
    };

    // Two deploy drones hold heavy standbys; a-deploy's is the older one.
    let h = Harness::start(scoped_disk_cfg(json!({
        "standby": {"classes": ["heavy"], "per_drone": 1}
    })))
    .await;
    let a = h.enroll("a-deploy").await;
    h.heartbeat_with(&a, deploy(100 * GIB)).await;
    let first = OffsetDateTime::now_utc();
    h.queen.tick(first).await.unwrap();
    let b = h.enroll("b-deploy").await;
    h.heartbeat_with(&b, deploy(100 * GIB)).await;
    let second = after(first);
    h.queen.tick(second).await.unwrap();
    let (held_a, held_b) = (
        standby_on(&h, "a-deploy").await,
        standby_on(&h, "b-deploy").await,
    );
    assert_eq!((held_a.len(), held_b.len()), (1, 1));

    // a-deploy falls below the 32 GiB heavy floor (above the global one) and
    // a heavy deploy job queues: b-deploy's standby starts.
    h.heartbeat_with(&a, deploy(20 * GIB)).await;
    h.queue_job(701, "borg-ci-heavy-deploy", 1);
    h.queen.tick(after(second)).await.unwrap();
    let mut beat = deploy(100 * GIB);
    beat["held_standby"] = json!(held_b);
    assert_eq!(
        h.heartbeat_with(&b, beat).await["start_standby"],
        json!(held_b)
    );
    let mut beat = deploy(20 * GIB);
    beat["held_standby"] = json!(held_a);
    assert_eq!(h.heartbeat_with(&a, beat).await["start_standby"], json!([]));

    // Only the blocked drone holds a standby: a runner is minted elsewhere.
    let h = Harness::start(scoped_disk_cfg(json!({
        "standby": {"classes": ["heavy"], "per_drone": 1}
    })))
    .await;
    let a = h.enroll("a-deploy").await;
    h.heartbeat_with(&a, deploy(100 * GIB)).await;
    let first = OffsetDateTime::now_utc();
    h.queen.tick(first).await.unwrap();
    assert_eq!(standby_on(&h, "a-deploy").await.len(), 1);
    h.heartbeat_with(&a, deploy(20 * GIB)).await;
    let b = h.enroll("b-deploy").await;
    h.heartbeat_with(&b, deploy(100 * GIB)).await;
    h.queue_job(702, "borg-ci-heavy-deploy", 1);
    h.queen.tick(after(first)).await.unwrap();
    let minted: Vec<_> = h
        .queen
        .store()
        .active_leases()
        .await
        .unwrap()
        .into_iter()
        .filter(|l| l.state == borg::store::LeaseStatus::Pending)
        .map(|l| (l.drone, l.class, l.scope))
        .collect();
    assert_eq!(
        minted,
        vec![(
            "b-deploy".to_owned(),
            borg::classes::Class::Heavy,
            borg::classes::Scope::Deploy
        )],
        "the blocked standby does not cover the job"
    );
}
