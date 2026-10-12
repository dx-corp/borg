//! Host execution, updates, and protocol tests share one fixture module.
//! Filter a module with `cargo test --test host_suite drone::` in the standalone
//! crate, or add `-p borg` from the Mono Rust workspace.

mod support;

mod admin_cli;
mod contract;
mod drone;
mod job_user;
mod rollout;
mod run;
mod tailnet;
mod update;
