//! Page modules. Every `*.rs` file in this directory is picked up by `build.rs`
//! and must define `pub fn register(r: &mut Registry)`; there is no list to edit.

include!(concat!(env!("OUT_DIR"), "/ui_pages.rs"));
