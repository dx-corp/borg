//! Single integration-test binary for `borg`: scheduling, service-state, host
//! execution, update, and protocol tests share one executable and one fixture
//! module. Filter a module with `cargo test --test suite units::` in the
//! standalone crate, or add `-p borg` from the Mono Rust workspace.
//! `sccache_slots` stays a separate binary because it drives real `cargo` and
//! `sccache` processes.

mod support;

mod admin_cli;
mod burst;
mod capacity;
mod contract;
mod drone;
mod explain;
mod explain_cli;
mod health;
mod hostdiag_wire;
mod janitor;
mod job_user;
mod placement_batch;
mod queen;
mod requeue;
mod rollout;
mod run;
mod tailnet;
mod units;
mod update;
mod wake;
