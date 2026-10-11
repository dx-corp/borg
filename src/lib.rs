//! borg: a self-organizing CI fleet on commodity hardware.
//!
//! * [`probe`], [`classes`] and [`allocator`] are the pure core shared by the
//!   drone and the queen.
//! * The drone ([`drone`]) runs on every machine: it heartbeats to the queen,
//!   runs each lease as an ephemeral runner and cleans up after it.
//! * The queen ([`queen`], [`github`], [`store`], [`api`], [`config`]) polls
//!   the GitHub Actions queue, decides what to lease where, mints single-use
//!   JIT runner configs and serves the drone API.

pub mod admin;
pub mod allocator;
pub mod api;
pub mod bid;
pub mod classes;
pub mod config;
pub mod configfile;
pub mod drone;
pub mod enroll;
pub mod envcontract;
pub mod github;
pub mod health;
pub mod homeostasis;
pub mod janitor;
pub mod probe;
pub mod profile;
pub mod queen;
pub mod reclaim;
pub mod rollout;
pub mod run;
pub mod schedule;
pub mod secret;
pub mod store;
pub mod tailnet;
pub mod telemetry;
pub mod units;
pub mod units_api;
pub mod update;
pub mod wake;
