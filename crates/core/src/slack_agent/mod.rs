//! Slack agent: Socket Mode transport and the pieces built on it.

pub mod approvals_bridge;
pub mod catchup;
pub mod executor;
pub mod install;
pub mod intake;
pub mod limits;
pub mod listener;
pub mod seen;
pub mod socket;
pub mod status;
#[cfg(test)]
pub mod testutil;
pub mod threads;
pub mod web;
