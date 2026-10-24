//! Shared logic for LoopX: config, contracts, and helpers used by the CLI and server.

pub mod agents;
pub mod ai_cli_config;
pub mod approvals;
pub mod auth_health;
pub mod budget;
pub mod bundled;
pub mod chat;
pub mod chat_eval;
pub mod chat_tools;
pub mod config;
pub mod connectors;
pub mod connectors_config;
pub mod content_archive;
pub mod cost;
pub mod doctor;
pub mod events;
pub mod gate_stats;
pub mod gitlab;
pub mod gitlab_cache;
pub mod gitlab_runner;
pub mod golden_eval;
pub mod health;
pub mod inbox_config;
pub mod inbox_runner;
pub mod inbox_seen;
pub mod inbox_status;
pub mod inbox_triage;
pub mod issue_tracking_config;
pub mod keychain;
pub mod launchd;
pub mod learning;
pub mod ledger;
pub mod loop_audit;
pub mod loop_definition;
pub mod loop_eval;
pub mod loop_notify;
pub mod loop_plugins;
pub mod loop_progress;
pub mod loop_result;
pub mod loop_runtime;
pub mod loop_scheduler;
pub mod loop_serialize;
pub mod loop_state;
pub mod loop_templates;
pub mod loop_verifiers;
pub mod loopkit;
pub mod loops_config;
pub mod mail_auth;
pub mod mail_http;
pub mod mail_mime;
pub mod mail_provider;
pub mod mail_sender;
pub mod mail_time;
pub mod memory_store;
pub mod messages;
pub mod metrics;
pub mod mr;
pub mod msg;
pub mod oauth;
pub mod policy;
pub mod proc;
pub mod prompt;
pub mod pydumps;
pub mod pyfmt_lite;
pub mod pyjson;
pub mod risk;
pub mod run_control;
pub mod run_history;
pub mod run_loop;
pub mod runner_util;
pub mod secrets;
pub mod seen_store;
pub mod settings_store;
pub mod sha256;
pub mod slack;
pub mod status;
pub mod topic_runner;
pub mod topic_seen;
pub mod topics;
pub mod update_check;
pub mod worktree;

/// The version of the `loopx` binary, taken from the workspace package version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_semver_like() {
        assert_eq!(VERSION.split('.').count(), 3);
    }
}
