mod support;

use std::net::{IpAddr, Ipv4Addr};

use borg::tailnet::{StaticIdentities, TailnetIdentity};
use serde_json::{Value, json};
use support::Harness;

fn identities(node: &str, tags: &[&str], user: Option<&str>) -> StaticIdentities {
    let mut m = StaticIdentities::default();
    m.0.insert(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        TailnetIdentity {
            node_name: node.to_owned(),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            user: user.map(str::to_owned),
        },
    );
    m
}

fn enabled() -> Value {
    json!({"tailnet_enroll": {"allowed_tags": ["tag:ci"], "allowed_users": ["dev@example.test"]}})
}

async fn enroll(h: &Harness) -> reqwest::Response {
    h.http
        .post(format!("{}/v1/enroll", h.url))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn an_allowlisted_tagged_node_gets_a_working_drone_token() {
    let h = Harness::start_with(
        enabled(),
        identities("Host_A.example.invalid.", &["tag:ci"], None),
    )
    .await;
    let resp = enroll(&h).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "host-a");
    let token = body["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("borg_"));

    // The issued token authenticates as that drone.
    assert_eq!(h.heartbeat(&token, &[]).await.status(), 200);
    assert_eq!(h.status().await["drones"][0]["name"], "host-a");
}

#[tokio::test]
async fn an_untagged_node_enrolls_only_for_an_allowed_user() {
    let h = Harness::start_with(
        enabled(),
        identities("laptop.example.invalid.", &[], Some("dev@example.test")),
    )
    .await;
    assert_eq!(enroll(&h).await.status(), 200);

    let h = Harness::start_with(
        enabled(),
        identities("laptop.example.invalid.", &[], Some("eve@example.test")),
    )
    .await;
    assert_eq!(enroll(&h).await.status(), 403);
}

#[tokio::test]
async fn nodes_outside_the_allowlist_or_the_tailnet_are_refused() {
    let h = Harness::start_with(
        enabled(),
        identities("host-a.example.invalid.", &["tag:other"], None),
    )
    .await;
    assert_eq!(enroll(&h).await.status(), 403);
    assert!(h.status().await["drones"].as_array().unwrap().is_empty());

    // Not in the verifier's table at all: not a tailnet node.
    let h = Harness::start_with(enabled(), StaticIdentities::default()).await;
    assert_eq!(enroll(&h).await.status(), 403);
}

#[tokio::test]
async fn a_second_enrollment_of_the_same_node_is_a_conflict_and_keeps_the_first_token() {
    let h = Harness::start_with(
        enabled(),
        identities("host-a.example.invalid.", &["tag:ci"], None),
    )
    .await;
    let first: Value = enroll(&h).await.json().await.unwrap();
    assert_eq!(enroll(&h).await.status(), 409);
    let token = first["token"].as_str().unwrap();
    assert_eq!(h.heartbeat(token, &[]).await.status(), 200);
}

#[tokio::test]
async fn an_unusable_node_name_is_unprocessable() {
    let h = Harness::start_with(
        enabled(),
        identities("---.example.invalid.", &["tag:ci"], None),
    )
    .await;
    assert_eq!(enroll(&h).await.status(), 422);
}

#[tokio::test]
async fn the_endpoint_does_not_exist_unless_configured() {
    let h = Harness::start_with(
        json!({}),
        identities("host-a.example.invalid.", &["tag:ci"], None),
    )
    .await;
    assert_eq!(enroll(&h).await.status(), 404);
}
