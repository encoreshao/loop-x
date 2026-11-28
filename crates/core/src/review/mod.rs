//! The review engine shared by the MR Review loop and, later, local review: diff text,
//! answer cleaning, taxonomy and Markdown rendering.
pub mod diff;
pub mod findings;
pub mod render;
pub mod taxonomy;
