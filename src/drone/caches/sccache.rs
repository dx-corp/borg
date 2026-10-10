//! Optional host-level sccache server, for hit statistics.
//!
//! sccache keeps its hit and miss counters in the server's memory only; a
//! stopped server leaves nothing on disk to read (a new server starts at zero).
//! Because a job's own server dies with its lease, the drone can only report
//! hit rates by running one long-lived server per host that all leases share.
//! That is opt-in (`caches.sccache.server`): the server's configuration then
//! applies to every job, and a job whose sccache version differs from this
//! binary's will restart it.

use crate::drone::exec::Exec;

/// Environment that configures (and addresses) the host server.
pub fn server_env(dir: &str, size: &str, port: u16) -> Vec<(String, String)> {
    vec![
        ("SCCACHE_DIR".to_owned(), dir.to_owned()),
        ("SCCACHE_CACHE_SIZE".to_owned(), size.to_owned()),
        ("SCCACHE_SERVER_PORT".to_owned(), port.to_string()),
        // The server must outlive any single job.
        ("SCCACHE_IDLE_TIMEOUT".to_owned(), "0".to_owned()),
    ]
}

/// Total cache hits and misses from `sccache --show-stats --stats-format json`.
pub fn parse_stats_json(json: &str) -> Option<(u64, u64)> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let sum = |key: &str| -> Option<u64> {
        let counts = value.get("stats")?.get(key)?.get("counts")?.as_object()?;
        Some(counts.values().filter_map(serde_json::Value::as_u64).sum())
    };
    Some((sum("cache_hits")?, sum("cache_misses")?))
}

fn with_env(env: &[(String, String)], bin: &str, args: &[&str]) -> (String, Vec<String>) {
    let mut argv: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    argv.push(bin.to_owned());
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    ("env".to_owned(), argv)
}

fn run(exec: &dyn Exec, env: &[(String, String)], bin: &str, args: &[&str]) -> Option<String> {
    let (program, argv) = with_env(env, bin, args);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    exec.run(&program, &argv)
        .ok()
        .filter(|o| o.success)
        .map(|o| o.stdout)
}

/// Start the host server. Succeeds when one is already running.
pub fn start_server(exec: &dyn Exec, bin: &str, env: &[(String, String)]) -> bool {
    run(exec, env, bin, &["--start-server"]).is_some()
}

/// Cumulative `(hits, misses)` of the host server, if sccache answers.
pub fn show_stats(exec: &dyn Exec, bin: &str, env: &[(String, String)]) -> Option<(u64, u64)> {
    parse_stats_json(&run(
        exec,
        env,
        bin,
        &["--show-stats", "--stats-format", "json"],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drone::cleanup::testing::FakeExec;

    // Shape captured from sccache 0.16.0.
    const SAMPLE: &str = r#"{"stats":{"compile_requests":8,"requests_executed":3,
        "cache_hits":{"counts":{"Rust":2,"C/C++":1},"adv_counts":{"rust":2}},
        "cache_misses":{"counts":{"Rust":4},"adv_counts":{"rust":4}}},"cache_size":9}"#;

    #[test]
    fn hits_and_misses_are_summed_over_languages() {
        assert_eq!(parse_stats_json(SAMPLE), Some((3, 4)));
        let empty = r#"{"stats":{"cache_hits":{"counts":{}},"cache_misses":{"counts":{}}}}"#;
        assert_eq!(parse_stats_json(empty), Some((0, 0)));
        for bad in ["", "{}", "[]", r#"{"stats":{}}"#, "not json"] {
            assert_eq!(parse_stats_json(bad), None, "{bad}");
        }
    }

    #[test]
    fn commands_run_through_env_with_the_server_configuration() {
        let x = FakeExec::default();
        let env = server_env("/c/sccache", "50G", 4999);
        x.on(
            "env SCCACHE_DIR=/c/sccache SCCACHE_CACHE_SIZE=50G SCCACHE_SERVER_PORT=4999 SCCACHE_IDLE_TIMEOUT=0 sccache --show-stats --stats-format json",
            SAMPLE,
        );
        assert!(start_server(&x, "sccache", &env));
        assert_eq!(show_stats(&x, "sccache", &env), Some((3, 4)));
        assert!(x.calls()[0].ends_with("sccache --start-server"));
    }

    #[test]
    fn a_missing_or_failing_sccache_is_just_no_stats() {
        let x = FakeExec::default();
        let env = server_env("/c", "1G", 1);
        x.fail("env SCCACHE_DIR=/c SCCACHE_CACHE_SIZE=1G SCCACHE_SERVER_PORT=1 SCCACHE_IDLE_TIMEOUT=0 sccache --start-server");
        assert!(!start_server(&x, "sccache", &env));
        assert_eq!(
            show_stats(&x, "sccache", &env),
            None,
            "empty stdout is not a stats document"
        );
    }
}
