//! Scheduling and service-state tests share one executable and fixture module.
//! Filter a module with `cargo test --test suite units::` in the standalone
//! crate, or add `-p borg` from the Mono Rust workspace.

mod support;

mod capacity;
mod health;
mod janitor;
mod requeue;
mod units;
mod wake;
