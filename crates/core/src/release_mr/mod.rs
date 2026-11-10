//! The release MR: an agent summarises what changed between two branches and the
//! MR is created from that (see docs/superpowers/specs/2026-11-10-release-mr-panel-design.md).

pub mod api;
pub mod authors;
pub mod collect;
pub mod job;
pub mod render;
pub mod run;
pub mod summarize;

use std::fmt;

/// Why a release MR could not be made. Nothing is created in any of these cases.
#[derive(Debug, Clone, PartialEq)]
pub enum ReleaseError {
    /// The source branch has nothing the target lacks.
    NothingToRelease,
    /// More commits than one release MR should summarise.
    TooLarge(usize),
    /// An MR for this source and target is already open (its URL).
    OpenMrExists(String),
    /// A branch moved since the preview was generated.
    BranchesMoved,
    Other(String),
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NothingToRelease => write!(
                f,
                "Nothing to release: the target already has every commit of the source"
            ),
            Self::TooLarge(n) => write!(f, "{n} commits is too many for one release MR"),
            Self::OpenMrExists(url) => write!(
                f,
                "A merge request for these branches is already open: {url}"
            ),
            Self::BranchesMoved => {
                write!(f, "The branches changed since this preview; generate again")
            }
            Self::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ReleaseError {}

impl From<String> for ReleaseError {
    fn from(m: String) -> Self {
        Self::Other(m)
    }
}
