//! Server-rendered pages, ported from the Python dashboard (`bin/web/*.py`) and
//! checked against it byte for byte by `tests/golden_ui/`.
//!
//! Layout:
//!
//! * foundation: [`i18n`], [`csrf`], [`pyfmt`] (Python-exact `html.escape`,
//!   `json.dumps`, `quote`, `parse_qs`, `str.format`), [`template`] and [`consts`]
//!   (the extracted f-string templates and Python constants, see
//!   `tools/extract_ui_assets.py`), [`assets`];
//! * page chrome: [`shell`] (`render_shell`), [`nav`] (sidebar), [`hub`] (tabbed
//!   hubs), [`badge`], [`logos`], [`markdown`], [`update`];
//! * plumbing: [`routes`] (request/response types and the route registry),
//!   [`state`] (paths, CSRF token, kill switches), [`glue`] (the axum bridge);
//! * [`pages`]: one module per page group, each registering its own routes.
//!
//! See `tests/golden_ui/README.md` for how to port a page.

pub mod assets;
pub mod badge;
pub mod consts;
pub mod csrf;
pub mod gitlab;
pub mod glue;
pub mod hub;
pub mod i18n;
pub mod logos;
pub mod markdown;
pub mod nav;
pub mod pages;
pub mod pyfmt;
pub mod routes;
pub mod shell;
pub mod state;
pub mod template;
pub mod update;

pub use routes::{Registry, UiCtx, UiRequest, UiResponse};
pub use state::UiState;
