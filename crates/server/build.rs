//! Generates two index files into OUT_DIR so that adding a UI asset or a page
//! module never means editing a shared file (the usual source of merge
//! conflicts when several people port pages at once):
//!
//! * `embedded_assets.rs`: every file under `assets/` as `(relative path, bytes)`.
//! * `ui_pages.rs`: a `mod` line for each `src/ui/pages/<name>.rs` and a
//!   `register_all` that calls each module's `register(&mut Registry)`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("out dir"));

    let assets = manifest.join("assets");
    let mut files = Vec::new();
    walk(&assets, &mut files);
    files.sort();
    let mut text = String::from("pub static EMBEDDED: &[(&str, &[u8])] = &[\n");
    for file in &files {
        let rel = file
            .strip_prefix(&assets)
            .expect("under assets")
            .to_string_lossy()
            .replace('\\', "/");
        let _ = writeln!(text, "    ({rel:?}, include_bytes!({:?})),", file.display());
    }
    text.push_str("];\n");
    std::fs::write(out_dir.join("embedded_assets.rs"), text).expect("write assets index");

    let pages = manifest.join("src").join("ui").join("pages");
    let mut modules: Vec<String> = std::fs::read_dir(&pages)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_stem()?.to_str()?.to_string();
            (p.extension()? == "rs" && name != "mod").then_some(name)
        })
        .collect();
    modules.sort();
    let mut text = String::new();
    for m in &modules {
        let _ = writeln!(
            text,
            "#[path = {:?}]\npub mod {m};",
            pages.join(format!("{m}.rs")).display()
        );
    }
    text.push_str("pub fn register_all(r: &mut crate::ui::routes::Registry) {\n");
    for m in &modules {
        let _ = writeln!(text, "    {m}::register(r);");
    }
    text.push_str("}\n");
    std::fs::write(out_dir.join("ui_pages.rs"), text).expect("write pages index");

    println!("cargo:rerun-if-changed=assets");
    println!("cargo:rerun-if-changed=src/ui/pages");
    println!("cargo:rerun-if-changed=build.rs");
}
