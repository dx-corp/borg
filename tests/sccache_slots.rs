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
    let port = free_port();
    let dir = tmp.path().join("sccache").to_string_lossy().into_owned();
    let flags = rustflags::remap_flags(&work, 2, &cargo_home).join(" ");

    let mut env = sccache::server_env(&dir, "1G", port);
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
    assert!(
        sccache::start_server(&SystemExec, "sccache", &env),
        "sccache server starts"
    );

    let build = |slot: usize| {
        let crate_dir = work.join(format!("slot-{slot}")).join("demo");
        write_crate(&crate_dir);
        let manifest = crate_dir.join("Cargo.toml");
        assert!(run_with(
            &env,
            "cargo",
            &[
                "build",
                "--lib",
                "--quiet",
                "--manifest-path",
                manifest.to_str().unwrap()
            ]
        ));
        sccache::show_stats(&SystemExec, "sccache", &env).expect("sccache answers")
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

    let _ = run_with(&env, "sccache", &["--stop-server"]);
}
