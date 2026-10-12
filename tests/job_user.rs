//! Real privilege separation. Needs root (to change user) and util-linux
//! `setpriv`; skipped otherwise. Run it with, for example,
//! `sudo -E cargo test --test host_suite job_user:: -- --nocapture`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use borg::drone::exec::{Exec, SystemExec};
use borg::drone::jobuser::{JobExec, JobUser};

/// Numeric id of `nobody`: present on every Linux system, unprivileged, and
/// in no interesting group.
const JOB_ID: &str = "65534";

fn out(exec: &dyn Exec, program: &str, args: &[&str]) -> (bool, String) {
    let o = exec.run(program, args).unwrap();
    (o.success, o.stdout)
}

#[test]
fn a_lease_runs_as_a_different_uid_without_access_to_the_drone_token_or_any_capability() {
    let (_, uid) = out(&SystemExec, "id", &["-u"]);
    if uid != "0"
        || !SystemExec
            .run("setpriv", &["--version"])
            .is_ok_and(|o| o.success)
    {
        println!("skipping: needs root and setpriv");
        return;
    }
    let drone_uid = uid;

    // Shaped like /etc/borg: a directory only the drone's user can enter.
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("etc-borg");
    fs::create_dir(&config_dir).unwrap();
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let token = config_dir.join("drone.token");
    fs::write(&token, "borg_secret_token\n").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    // Let the job user at least reach the temporary parent.
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();

    let job = JobExec::new(
        Arc::new(SystemExec),
        JobUser {
            name: JOB_ID.to_owned(),
            home: "/nonexistent".into(),
            priv_bin: "setpriv".to_owned(),
        },
    );

    let (ok, lease_uid) = out(&job, "id", &["-u"]);
    assert!(ok, "the lease command ran");
    assert_eq!(lease_uid, JOB_ID);
    assert_ne!(
        lease_uid, drone_uid,
        "the lease runs as a different uid than the drone"
    );

    let token_path = token.to_string_lossy().into_owned();
    let (read_ok, contents) = out(&job, "cat", &[&token_path]);
    assert!(
        !read_ok && !contents.contains("secret"),
        "the job user cannot read the token"
    );
    let (listed, _) = out(&job, "ls", &[&config_dir.to_string_lossy()]);
    assert!(!listed, "nor list the drone's config directory");
    // The drone itself can, which is the point of the split.
    assert_eq!(
        fs::read_to_string(&token).unwrap().trim(),
        "borg_secret_token"
    );

    // No capability survives into the job.
    let (_, caps) = out(
        &job,
        "sh",
        &[
            "-c",
            "grep -E '^(CapEff|CapPrm|CapAmb|CapBnd):' /proc/self/status",
        ],
    );
    for line in caps.lines() {
        assert!(
            line.ends_with("0000000000000000"),
            "capabilities remain: {line}"
        );
    }
    println!(
        "lease uid {lease_uid} vs drone uid {drone_uid}; token unreadable; capabilities dropped"
    );
}
