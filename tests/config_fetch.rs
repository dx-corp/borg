//! `borg config fetch`: the queen config is read from the private repository
//! with a contents-read token scoped to that one repository.

use crate::support;

use serde_json::json;
use support::Harness;

fn queen_cfg(h: &Harness, key: &std::path::Path, with_app: bool) -> borg::config::QueenConfig {
    let mut cfg = json!({
        "github": {"org": support::ORG, "app_id": "999", "api_base": h.gh_url},
        "repos": [format!("{}/app", support::ORG)],
        "db_path": "/tmp/q.db",
    });
    if with_app {
        cfg["mirror_auth"] =
            json!({"app_id": "12345", "private_key_path": key, "repositories": ["app"]});
    }
    borg::config::QueenConfig::from_json(&cfg.to_string()).unwrap()
}

#[tokio::test]
async fn fetch_reads_the_file_at_the_ref_with_a_contents_read_token_for_that_repo() {
    let h = Harness::start(json!({})).await;
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("mirror-app.pem");
    std::fs::write(&key, &h.private_pem).unwrap();
    let path = format!(
        "/repos/{}/mono/contents/config/borg/queen.json",
        support::ORG
    );
    h.state()
        .http_replies
        .insert(path.clone(), (200, vec![], "{\"repos\":[]}\n".into()));

    let cfg = queen_cfg(&h, &key, true);
    let bytes = borg::configfetch::fetch(&cfg, "mono", "main", "config/borg/queen.json")
        .await
        .unwrap();
    assert_eq!(bytes, b"{\"repos\":[]}\n");
    // The token is scoped to contents: read on mono only, not the
    // mirror_auth repositories.
    let scoped = h.state().scoped_requests.clone();
    assert_eq!(
        scoped.last().unwrap(),
        &json!({"permissions": {"contents": "read"}, "repositories": ["mono"]})
    );

    let out = dir.path().join("queen.json.new");
    borg::configfetch::write_atomic(&out, &bytes).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), bytes);

    // A missing file is an error, never an empty config.
    h.state()
        .http_replies
        .insert(path, (404, vec![], String::new()));
    let err = borg::configfetch::fetch(&cfg, "mono", "main", "config/borg/queen.json")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("404"), "{err}");

    // Without a contents-read App there is nothing to read with.
    let err = borg::configfetch::fetch(&queen_cfg(&h, &key, false), "mono", "main", "x")
        .await
        .unwrap_err();
    assert!(matches!(err, borg::configfetch::FetchError::NoApp), "{err}");
}
