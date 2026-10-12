//! Compiles one small crate through sccache in slot directories, to show what
//! slot-stable work directories do for the compile cache. Skipped when `cargo`
//! or `sccache` is not installed.

use std::fs;
use std::path::Path;

use borg::drone::caches::{rustflags, sccache};
use borg::drone::exec::{Exec, SystemExec};

fn available(program: &str) -> bool {
    SystemExec
        .run(program, &["--version"])
        .is_ok_and(|o| o.success)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Run `program args` under `env`, through `env(1)` since the executor takes no environment.
fn run_with(env: &[(String, String)], program: &str, args: &[&str]) -> bool {
    let mut argv: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    argv.push(program.to_owned());
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    SystemExec.run("env", &argv).is_ok_and(|o| o.success)
}

struct TestServer {
    env: Vec<(String, String)>,
}

impl TestServer {
    fn start(root: &Path) -> Self {
        let env = sccache::server_env(&root.join("cache").to_string_lossy(), "1G", free_port());
        // sccache gives an inherited Unix socket precedence over the TCP port.
        // Keep start, compiler, stats and cleanup commands on our own endpoint.
        #[cfg(unix)]
        let env = {
            let mut env = env;
            env.push((
                "SCCACHE_SERVER_UDS".to_owned(),
                root.join("socket").to_string_lossy().into_owned(),
            ));
            env
        };
        let server = Self { env };
        assert!(
            sccache::start_server(&SystemExec, "sccache", &server.env),
            "sccache server starts"
        );
        server
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = run_with(&self.env, "sccache", &["--stop-server"]);
    }
}

fn write_crate(dir: &Path) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        "pub fn add(a: u64, b: u64) -> u64 { a + b }\npub fn fail() { panic!(\"boom\") }\n",
    )
    .unwrap();
}

#[test]
fn workspace_crates_hit_the_compile_cache_when_a_slot_is_reused() {
    if !(available("cargo") && available("sccache")) {
        println!("skipping: cargo and sccache are required");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let cargo_home = tmp.path().join("cargo");
    let flags = rustflags::remap_flags(&work, 2, &cargo_home).join(" ");

    let mut server = TestServer::start(tmp.path());
    let env = &mut server.env;
    env.extend([
        ("RUSTC_WRAPPER".to_owned(), "sccache".to_owned()),
        ("CARGO_INCREMENTAL".to_owned(), "0".to_owned()),
        (
            "CARGO_TARGET_DIR".to_owned(),
            tmp.path().join("target").to_string_lossy().into_owned(),
        ),
        (
            "CARGO_HOME".to_owned(),
            cargo_home.to_string_lossy().into_owned(),
        ),
        ("RUSTFLAGS".to_owned(), flags),
    ]);
    let build = |slot: usize| {
        let crate_dir = work.join(format!("slot-{slot}")).join("demo");
        write_crate(&crate_dir);
        let manifest = crate_dir.join("Cargo.toml");
        assert!(run_with(
            env,
            "cargo",
            &[
                "build",
                "--lib",
                "--quiet",
                "--manifest-path",
                manifest.to_str().unwrap()
            ]
        ));
        sccache::show_stats(&SystemExec, "sccache", env).expect("sccache answers")
    };

    let first = build(0);
    fs::remove_dir_all(work.join("slot-0")).unwrap(); // what drone cleanup does between leases
    let reused = build(0);
    let other = build(1);
    println!(
        "slot-0 first compile:   hits={} misses={}",
        first.0, first.1
    );
    println!(
        "slot-0 reused (cleaned): hits={} misses={}",
        reused.0, reused.1
    );
    println!(
        "slot-1 (other slot):    hits={} misses={}",
        other.0, other.1
    );

    assert_eq!(first.0, 0, "a cold cache cannot hit");
    assert!(first.1 >= 1);
    assert!(
        reused.0 > first.0,
        "recompiling in a reused slot hits: {first:?} -> {reused:?}"
    );
    // The other slot is deliberately not asserted: sccache hashes the compile's
    // working directory and every CARGO_* variable (CARGO_MANIFEST_DIR), which
    // differ between slots whatever rustc flags are passed. The numbers above
    // record what happens.
}

#[cfg(unix)]
#[test]
fn slot_fixture_preserves_an_inherited_sccache_server() {
    if !(available("cargo") && available("rustc") && available("sccache")) {
        println!("skipping: cargo, rustc and sccache are required");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let outer = TestServer::start(tmp.path());
    let source = tmp.path().join("sentinel.rs");
    fs::write(&source, "pub fn sentinel() -> u8 { 7 }\n").unwrap();
    assert!(run_with(
        &outer.env,
        "sccache",
        &[
            "rustc",
            "--crate-name",
            "sentinel",
            "--crate-type",
            "lib",
            "--emit=dep-info,link",
            source.to_str().unwrap(),
            "--out-dir",
            tmp.path().to_str().unwrap(),
        ],
    ));
    let before = sccache::show_stats(&SystemExec, "sccache", &outer.env).unwrap();
    assert!(before.1 > 0, "the outer server has a sentinel cache miss");

    // Use a child process so ambient environment is exercised without changing
    // this test process's environment while other tests may be running.
    assert!(run_with(
        &outer.env,
        std::env::current_exe().unwrap().to_str().unwrap(),
        &[
            "--exact",
            "workspace_crates_hit_the_compile_cache_when_a_slot_is_reused",
            "--nocapture",
        ],
    ));
    assert_eq!(
        sccache::show_stats(&SystemExec, "sccache", &outer.env),
        Some(before),
        "the outer server survives fixture cleanup with unchanged counters"
    );
}
