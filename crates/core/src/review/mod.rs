//! The review engine shared by the MR Review loop and, later, local review: diff text,
//! answer cleaning, taxonomy and Markdown rendering.
pub mod config;
pub mod context;
pub mod diff;
pub mod findings;
pub mod glob;
pub mod ledger;
pub mod render;
pub mod semgrep_rules;
pub mod taxonomy;
pub mod tools;
pub mod verify;
