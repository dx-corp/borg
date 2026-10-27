use crate::support;

use serde_json::{Value, json};
use support::Harness;
use time::OffsetDateTime;

fn config() -> Value {
    json!({
        "drone_expiry": {"burst_prefixes": ["gcp-burst-"]},
        "capacity_alarm": {"enabled": false},
        "burst": {
            "enabled": true, "max_drones": 2,
            "budget": {"drone_hours_per_day": 24, "drone_hours_per_month": 150},
            "scale_up": {"classes": ["heavy"], "weights": {"main": 3, "pr": 1},
                         "min_weighted_unmet": 6, "sustained_secs": 600},
            "scale_down": {"idle_secs": 900},
            "gce": {"project": "p", "network": "borg-burst", "subnet": "borg-burst-usw1",
                    "zones": ["us-west1-b", "us-west1-a"], "machine_type": "c2d-highcpu-56"}
        }
    })
}

async fn status_at(h: &Harness, at: OffsetDateTime) -> Value {
    let s = borg::api::build_status(&h.queen, at.unix_timestamp())
        .await
        .unwrap();
    serde_json::to_value(s).unwrap()
}

fn burst_events(s: &Value) -> Vec<String> {
    s["health_events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["drone"] == "burst")
        .map(|e| e["event"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn burst_is_absent_from_status_when_not_configured() {
    let h = Harness::start(json!({})).await;
    h.tick().await;
    let s = h.status().await;
    assert!(s["burst"].is_null());
    assert!(!h.metrics().await.contains("borg_burst_"));
}

#[tokio::test]
async fn dry_run_logs_what_it_would_create_drain_and_delete_and_creates_nothing() {
    let h = Harness::start(config()).await;
    // Two main heavy jobs and no drone: weighted unmet 6.
    h.queue_job(1, "borg-ci-heavy", 5);
    h.queue_job(2, "borg-ci-heavy", 5);
    let t0 = OffsetDateTime::now_utc().replace_time(time::Time::from_hms(12, 0, 0).unwrap());
    let at = |s: i64| t0 + time::Duration::seconds(s);

    h.queen.tick(at(0)).await.unwrap();
    let s = status_at(&h, at(0)).await;
    assert_eq!(s["burst"]["dry_run"], true);
    assert_eq!(s["burst"]["demand"]["weighted_unmet"], 6);
    assert_eq!(s["burst"]["outcome"], "sustaining");
    assert!(burst_events(&s).is_empty());

    h.queen.tick(at(600)).await.unwrap();
    let s = status_at(&h, at(600)).await;
    assert_eq!(s["burst"]["outcome"], "create", "{}", s["burst"]);
    assert_eq!(s["burst"]["drones"]["managed"], 1);
    assert_eq!(s["burst"]["vms"][0]["zone"], "us-west1-b");
    assert_eq!(burst_events(&s), ["burst_would_create"]);
    // Nothing real: no drone was enrolled and no runner was minted.
    assert_eq!(s["drones"], json!([]));
    assert!(h.state().mints.is_empty());

    h.queen.tick(at(660)).await.unwrap();
    assert_eq!(
        status_at(&h, at(660)).await["burst"]["outcome"],
        "provisioning"
    );

    // Demand ends; the simulated drone enrolls, sits idle, is drained, then deleted.
    h.state().jobs.clear();
    for t in [1400, 2300, 2310] {
        h.queen.tick(at(t)).await.unwrap();
    }
    let s = status_at(&h, at(2310)).await;
    assert_eq!(
        burst_events(&s),
        [
            "burst_would_delete",
            "burst_would_drain",
            "burst_would_create"
        ],
        "newest first"
    );
    assert_eq!(s["burst"]["drones"]["managed"], 0);
    assert!(s["burst"]["spend"]["day_hours"].as_f64().unwrap() > 0.4);
    assert_eq!(s["burst"]["outcomes"]["no_demand"], 3);
    assert_eq!(s["burst"]["recent"].as_array().unwrap().len(), 3);

    let m = h.metrics().await;
    for line in [
        "borg_burst_dry_run 1",
        "borg_burst_drones{kind=\"managed\"} 0",
        "borg_burst_budget_drone_hours{window=\"day\"} 24",
        "borg_burst_passes{outcome=\"create\"} 1",
    ] {
        assert!(m.contains(line), "missing {line}\n{m}");
    }
}

#[tokio::test]
async fn drones_made_by_hand_count_toward_the_cap() {
    let h = Harness::start(config()).await;
    for name in ["gcp-burst-3", "gcp-burst-4"] {
        let t = h.enroll(name).await;
        h.heartbeat(&t, &[]).await;
    }
    // More heavy main jobs than the two test drones hold (one heavy each).
    // The cap needs no sustained window: it is checked first.
    for i in 0..10 {
        h.queue_job(i, "borg-ci-heavy", 5);
    }
    let t0 = OffsetDateTime::now_utc();
    let t1 = t0 + time::Duration::seconds(30);
    h.queen.tick(t0).await.unwrap();
    h.queen.tick(t1).await.unwrap();
    let s = status_at(&h, t1).await;
    assert_eq!(s["burst"]["drones"]["unmanaged"], 2, "{}", s["burst"]);
    assert_eq!(s["burst"]["outcome"], "at_max_drones");
    assert!(burst_events(&s).is_empty());
}

fn live_config() -> Value {
    let mut c = config();
    c["burst"]["dry_run"] = json!(false);
    c["burst"]["scale_down"]["provision_grace_secs"] = json!(1500);
    // Heartbeats happen at wall-clock time while ticks use a fixed clock.
    c["drone_stale_secs"] = json!(1_000_000);
    c
}

fn metadata_item(instance: &Value, key: &str) -> Option<String> {
    instance["body"]["metadata"]["items"]
        .as_array()?
        .iter()
        .find(|i| i["key"] == key)
        .and_then(|i| i["value"].as_str())
        .map(str::to_owned)
}

#[tokio::test]
async fn bootstrap_assets_are_served_only_while_burst_is_live() {
    let dry = Harness::start(config()).await;
    for path in ["/bootstrap/borg", "/bootstrap/toolchain.sh"] {
        let r = dry
            .http
            .get(format!("{}{path}", dry.url))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{path}");
    }
    let live = Harness::start(live_config()).await;
    let r = live
        .http
        .get(format!("{}/bootstrap/toolchain.sh", live.url))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), support::TOOLCHAIN_SCRIPT);
    let r = live
        .http
        .get(format!("{}/bootstrap/borg", live.url))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "the queen serves its own executable");
    assert!(r.content_length().unwrap_or(1) > 0);
}

#[tokio::test]
async fn live_burst_creates_scrubs_drains_and_deletes_a_real_drone() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queen.db");
    let h = Harness::start_file(live_config(), &db).await;
    h.queue_job(1, "borg-ci-heavy", 5);
    h.queue_job(2, "borg-ci-heavy", 5);
    let t0 = OffsetDateTime::now_utc().replace_time(time::Time::from_hms(12, 0, 0).unwrap());
    let at = |s: i64| t0 + time::Duration::seconds(s);

    h.queen.tick(at(0)).await.unwrap();
    let s = status_at(&h, at(0)).await;
    assert_eq!(s["burst"]["dry_run"], false);
    assert_eq!(s["burst"]["outcome"], "sustaining");
    assert!(h.state().gce_instances.is_empty());

    // Demand sustained: the queen enrolls a drone and creates its VM.
    h.queen.tick(at(600)).await.unwrap();
    let s = status_at(&h, at(600)).await;
    assert_eq!(s["burst"]["outcome"], "create", "{}", s["burst"]);
    assert_eq!(burst_events(&s), ["burst_create"]);
    let drone_name = s["burst"]["vms"][0]["drone"].as_str().unwrap().to_owned();
    let vm_name = s["burst"]["vms"][0]["vm"].as_str().unwrap().to_owned();
    assert!(drone_name.starts_with("gcp-burst-q"), "{drone_name}");
    assert_eq!(vm_name, drone_name.replacen("gcp-", "borg-", 1));
    assert!(
        s["drones"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == drone_name),
        "enrolled, not yet heartbeating: {}",
        s["drones"]
    );
    let instance = h.state().gce_instances[0].clone();
    assert_eq!(instance["name"], vm_name);
    assert_eq!(instance["zone"], "us-west1-b");
    let body = &instance["body"];
    assert_eq!(body["scheduling"]["provisioningModel"], "SPOT");
    assert_eq!(body["scheduling"]["instanceTerminationAction"], "DELETE");
    assert_eq!(body["serviceAccounts"], json!([]));
    assert_eq!(
        body["machineType"],
        "zones/us-west1-b/machineTypes/c2d-highcpu-56"
    );
    let script = metadata_item(&instance, "startup-script").unwrap();
    assert!(script.contains("QUEEN_URL='https://queen.example.test'"));
    assert!(script.contains(&format!("DRONE='{drone_name}'")));
    assert_eq!(
        metadata_item(&instance, "borg-ts-authkey").as_deref(),
        Some(support::TS_AUTHKEY)
    );
    let token = metadata_item(&instance, "borg-enroll-token").unwrap();
    assert!(h.state().gce_token_requests >= 1);
    assert!(
        h.state().mints.is_empty(),
        "no runner before the drone exists"
    );
    assert!(h.metrics().await.contains("borg_burst_dry_run 0"));

    h.queen.tick(at(660)).await.unwrap();
    assert_eq!(
        status_at(&h, at(660)).await["burst"]["outcome"],
        "provisioning"
    );

    // Demand ends before the VM comes up, so no lease lands on it. It
    // bootstraps and heartbeats with the token from its metadata: the queen
    // scrubs the secrets from the instance and keeps the script.
    h.state().jobs.clear();
    assert!(h.heartbeat(&token, &[]).await.status().is_success());
    h.queen.tick(at(720)).await.unwrap();
    let s = status_at(&h, at(720)).await;
    assert!(
        burst_events(&s).contains(&"burst_scrub".to_owned()),
        "{:?}",
        burst_events(&s)
    );
    assert!(s["burst"]["vms"][0]["scrubbed_at"].is_i64());
    let scrubs = h.state().gce_set_metadata.clone();
    assert_eq!(scrubs.len(), 1);
    let kept: Vec<&str> = scrubs[0]["body"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap())
        .collect();
    assert_eq!(kept, ["startup-script"]);
    assert_eq!(s["burst"]["drones"]["managed"], 1);
    assert_eq!(s["burst"]["drones"]["provisioning"], 0);

    // The VM list is persisted: a fresh queen over the same store sees it.
    let again = Harness::start_file(live_config(), &db).await;
    again.queen.tick(at(730)).await.unwrap();
    let s2 = status_at(&again, at(730)).await;
    assert_eq!(s2["burst"]["drones"]["managed"], 1, "{}", s2["burst"]);
    assert_eq!(s2["burst"]["vms"][0]["drone"], drone_name);
    drop(again);

    // Idle for 900s: the drone is drained (quarantined), then the VM and
    // the drone record go away.
    h.queen.tick(at(1400)).await.unwrap();
    h.queen.tick(at(2300)).await.unwrap();
    let s = status_at(&h, at(2300)).await;
    assert_eq!(burst_events(&s)[0], "burst_drain", "{:?}", burst_events(&s));
    let d = s["drones"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == drone_name)
        .unwrap();
    assert_eq!(d["quarantined"], true, "{d}");
    assert!(h.state().gce_deleted.is_empty());

    h.queen.tick(at(2310)).await.unwrap();
    let s = status_at(&h, at(2310)).await;
    assert_eq!(
        burst_events(&s)[0],
        "burst_delete",
        "{:?}",
        burst_events(&s)
    );
    assert_eq!(h.state().gce_deleted, std::slice::from_ref(&vm_name));
    assert!(
        !s["drones"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == drone_name),
        "drone record removed with its VM"
    );
    assert_eq!(s["burst"]["drones"]["managed"], 0);
    assert!(s["burst"]["spend"]["day_hours"].as_f64().unwrap() > 0.4);
    assert!(h.heartbeat(&token, &[]).await.status().is_client_error());
}

#[tokio::test]
async fn a_failed_create_undoes_the_enrollment_and_moves_to_another_zone() {
    let h = Harness::start(live_config()).await;
    h.queue_job(1, "borg-ci-heavy", 5);
    h.queue_job(2, "borg-ci-heavy", 5);
    let t0 = OffsetDateTime::now_utc().replace_time(time::Time::from_hms(12, 0, 0).unwrap());
    let at = |s: i64| t0 + time::Duration::seconds(s);
    h.state().gce_fail_create = Some("ZONE_RESOURCE_POOL_EXHAUSTED".to_owned());

    h.queen.tick(at(0)).await.unwrap();
    h.queen.tick(at(600)).await.unwrap();
    let s = status_at(&h, at(600)).await;
    assert_eq!(burst_events(&s), ["burst_create_failed"]);
    assert_eq!(s["burst"]["drones"]["managed"], 0);
    assert!(h.state().gce_instances.is_empty());
    assert_eq!(
        s["drones"],
        json!([]),
        "the enrollment made for the VM is undone"
    );

    // Demand still holds: the next create, a sustained window later, lands
    // in the other zone while the first cools down.
    h.queen.tick(at(1200)).await.unwrap();
    let s = status_at(&h, at(1200)).await;
    assert_eq!(
        burst_events(&s)[0],
        "burst_create",
        "{:?}",
        burst_events(&s)
    );
    assert_eq!(h.state().gce_instances[0]["zone"], "us-west1-a");
}

#[tokio::test]
async fn a_drone_shedding_under_pressure_does_not_mask_burst_demand() {
    let mut cfg = config();
    cfg["admission"] = json!({"psi_soft_pm": 300, "psi_light_only_pm": 500, "psi_hard_pm": 600,
                              "facts_max_age_secs": 100_000});
    let h = Harness::start(cfg).await;
    let now = OffsetDateTime::now_utc();
    let token = h.enroll("hot").await;
    h.heartbeat_with(
        &token,
        json!({"env_facts": {"observed_at": now.unix_timestamp(), "cpu_psi_some_avg10_pm": 540}}),
    )
    .await;
    // Two main heavy jobs the light-only drone will not take: weighted 6.
    h.queue_job(1, "borg-ci-heavy", 5);
    h.queue_job(2, "borg-ci-heavy", 5);
    h.queen.tick(now).await.unwrap();
    let s = status_at(&h, now).await;
    assert_eq!(s["drones"][0]["admission"]["state"], "light_only");
    assert_eq!(s["burst"]["demand"]["weighted_unmet"], 6, "{}", s["burst"]);
    assert_eq!(s["burst"]["demand"]["holds"], true);
    assert_eq!(s["burst"]["outcome"], "sustaining");
    assert!(h.state().mints.is_empty());
}

#[tokio::test]
async fn room_this_pass_fills_does_not_mask_burst_demand() {
    let h = Harness::start(config()).await;
    // One drone with room for one heavy lease and three main heavy jobs: the
    // pass grants the one slot, leaving two jobs (weighted 6) and no room.
    let token = h.enroll("local").await;
    h.heartbeat(&token, &[]).await;
    for i in 1..=3 {
        h.queue_job(i, "borg-ci-heavy", 5);
    }
    let now = OffsetDateTime::now_utc();
    h.queen.tick(now).await.unwrap();
    let s = status_at(&h, now).await;
    assert_eq!(h.state().mints.len(), 1, "{}", s["unmet"]);
    assert_eq!(s["capacity"]["heavy"]["free_slots"], 0, "{}", s["capacity"]);
    assert_eq!(s["burst"]["demand"]["weighted_unmet"], 6, "{}", s["burst"]);
    assert_eq!(s["burst"]["demand"]["holds"], true);
    assert_eq!(s["burst"]["outcome"], "sustaining");
}

#[tokio::test]
async fn a_create_whose_wait_fails_is_recorded_before_the_wait_and_keeps_its_enrollment() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("queen.db");
    let h = Harness::start_file(live_config(), &db).await;
    h.queue_job(1, "borg-ci-heavy", 5);
    h.queue_job(2, "borg-ci-heavy", 5);
    // The insert succeeds; the wait is refused (the 411 seen live).
    h.state().gce_wait_status = Some(411);
    let t0 = OffsetDateTime::now_utc().replace_time(time::Time::from_hms(12, 0, 0).unwrap());
    let at = |s: i64| t0 + time::Duration::seconds(s);
    h.queen.tick(at(0)).await.unwrap();
    h.queen.tick(at(600)).await.unwrap();
    let s = status_at(&h, at(600)).await;
    assert_eq!(burst_events(&s), ["burst_create"], "{}", s["burst"]);
    assert_eq!(s["burst"]["drones"]["managed"], 1, "{}", s["burst"]);
    let drone = s["burst"]["vms"][0]["drone"].as_str().unwrap().to_owned();
    assert!(
        s["drones"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == drone),
        "the enrollment stays: the VM exists and will use it"
    );
    let instance = h.state().gce_instances[0].clone();
    let token = metadata_item(&instance, "borg-enroll-token").unwrap();

    // A restarted queen sees the record and the cap holds.
    let again = Harness::start_file(live_config(), &db).await;
    again.queen.tick(at(660)).await.unwrap();
    let s2 = status_at(&again, at(660)).await;
    assert_eq!(s2["burst"]["vms"][0]["drone"], drone, "{}", s2["burst"]);
    assert_eq!(s2["burst"]["drones"]["managed"], 1);
    drop(again);

    // The drone comes up with its token and is served.
    assert!(h.heartbeat(&token, &[]).await.status().is_success());
}

/// Two labeled VMs the queen has no record of (the 10/24 orphans after a
/// restart): `instance` as the fake GCE holds it.
fn orphan(name: &str, zone: &str) -> Value {
    json!({"name": name, "zone": zone, "body": {"labels": {
        "borg-burst": "queen", "borg-drone": name.replacen("borg-", "gcp-", 1)}}})
}

#[tokio::test]
async fn unrecorded_burst_vms_are_found_by_label_counted_and_deleted() {
    let h = Harness::start(live_config()).await;
    for i in 0..10 {
        h.queue_job(i, "borg-ci-heavy", 5);
    }
    {
        let mut st = h.state();
        st.gce_instances = vec![
            orphan("borg-burst-q1c7f304", "us-west1-a"),
            orphan("borg-burst-q1c7f30e", "us-west1-a"),
        ];
        st.gce_fail_delete = true;
    }
    let t0 = OffsetDateTime::now_utc().replace_time(time::Time::from_hms(12, 0, 0).unwrap());
    let at = |s: i64| t0 + time::Duration::seconds(s);

    // Deletes fail: both still count against max_drones (2), so sustained
    // demand creates nothing more.
    h.queen.tick(at(0)).await.unwrap();
    h.queen.tick(at(600)).await.unwrap();
    let s = status_at(&h, at(600)).await;
    assert!(h.state().gce_lists >= 2);
    assert_eq!(s["burst"]["drones"]["managed"], 2, "{}", s["burst"]);
    assert_eq!(s["burst"]["outcome"], "at_max_drones", "{}", s["burst"]);
    assert_eq!(h.state().gce_instances.len(), 2, "no new VM past the cap");
    assert!(
        burst_events(&s).contains(&"burst_delete_failed".to_owned()),
        "{:?}",
        burst_events(&s)
    );

    // The next pass retries and both go. The cap is free again, and the
    // demand that held all along gets a real drone in the same pass.
    h.state().gce_fail_delete = false;
    h.queen.tick(at(660)).await.unwrap();
    let s = status_at(&h, at(660)).await;
    let mut deleted = h.state().gce_deleted.clone();
    deleted.sort();
    assert_eq!(deleted, ["borg-burst-q1c7f304", "borg-burst-q1c7f30e"]);
    assert_eq!(s["burst"]["outcome"], "create", "{}", s["burst"]);
    let live: Vec<&Value> = s["burst"]["vms"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["ended_at"].is_null())
        .collect();
    assert_eq!(live.len(), 1, "{}", s["burst"]);
    assert_ne!(live[0]["vm"], "borg-burst-q1c7f304");
    assert_ne!(live[0]["vm"], "borg-burst-q1c7f30e");
}

#[tokio::test]
async fn an_unrecorded_vm_whose_drone_is_enrolled_is_adopted_not_deleted() {
    let h = Harness::start(live_config()).await;
    let vm = "borg-burst-q1c7f304";
    h.enroll("gcp-burst-q1c7f304").await;
    h.state().gce_instances = vec![orphan(vm, "us-west1-a")];
    let now = OffsetDateTime::now_utc();
    h.queen.tick(now).await.unwrap();
    let s = status_at(&h, now).await;
    assert!(h.state().gce_deleted.is_empty());
    assert_eq!(s["burst"]["drones"]["managed"], 1, "{}", s["burst"]);
    assert_eq!(s["burst"]["vms"][0]["vm"], vm);
    assert_eq!(s["burst"]["vms"][0]["zone"], "us-west1-a");
}
