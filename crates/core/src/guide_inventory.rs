//! The file list a guide build plans from: every tracked file of the checkout with its
//! size, and whether its text may be read. LoopX reads files, never the model, so this
//! is where the readable/unreadable line is drawn.

use std::path::Path;
use std::process::Command;

pub const MAX_PATHS: usize = 6000;
pub const MAX_READ_BYTES: u64 = 256 * 1024;
pub const MAX_SCHEMA_BYTES: u64 = 2 * 1024 * 1024;
const SKIP_DIRS: [&str; 6] = [".git", "node_modules", "target", "vendor", ".venv", "dist"];
const BINARY_EXTS: [&str; 24] = [
    "png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "zip", "gz", "tgz", "tar", "jar", "woff",
    "woff2", "ttf", "otf", "eot", "mp3", "mp4", "mov", "wasm", "so", "dylib", "class",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub readable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub entries: Vec<Entry>,
    pub truncated: bool,
}

impl Inventory {
    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.path == path)
    }

    pub fn readable(&self, path: &str) -> bool {
        self.get(path).is_some_and(|e| e.readable)
    }

    /// One line per path (`path (12 KB)` or `path (not readable)`), alphabetical, cut
    /// deepest paths first until the text fits `budget` bytes.
    pub fn render(&self, budget: usize) -> String {
        let line = |e: &Entry| {
            if e.readable {
                format!("{} ({} KB)\n", e.path, e.size.div_ceil(1024).max(1))
            } else {
                format!("{} (not readable)\n", e.path)
            }
        };
        let mut keep: Vec<&Entry> = self.entries.iter().collect();
        let mut total: usize = keep.iter().map(|e| line(e).len()).sum();
        // Deepest first: drop from the end of a depth-descending order.
        let mut by_depth: Vec<usize> = (0..keep.len()).collect();
        by_depth.sort_by_key(|&i| std::cmp::Reverse((depth(&keep[i].path), keep[i].path.clone())));
        let mut dropped = vec![false; keep.len()];
        for i in by_depth {
            if total <= budget {
                break;
            }
            total -= line(keep[i]).len();
            dropped[i] = true;
        }
        let mut out = String::new();
        for (i, e) in keep.drain(..).enumerate() {
            if !dropped[i] {
                out.push_str(&line(e));
            }
        }
        out
    }
}

fn depth(path: &str) -> usize {
    path.matches('/').count()
}

fn is_env_file(name: &str) -> bool {
    name.starts_with(".env") || name.ends_with(".env")
}

/// Files whose structure the Data model page may read in full (up to 2 MB).
pub fn is_schema_like(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(
        name,
        "schema.rb" | "structure.sql" | "schema.prisma" | "schema.sql"
    ) || path.ends_with("db/schema.rb")
}

fn looks_binary(full: &Path, ext_binary: bool) -> bool {
    if ext_binary {
        return true;
    }
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(full) else {
        return true;
    };
    let mut head = [0u8; 8192];
    let n = f.read(&mut head).unwrap_or(0);
    head[..n].contains(&0)
}

fn tracked_paths(work_dir: &Path) -> Option<Vec<String>> {
    // Only trust git when work_dir is itself the repository root; inside a plain directory
    // under some enclosing repository `ls-files` would list that repository's files.
    let top = Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !top.status.success() {
        return None;
    }
    let top = std::fs::canonicalize(String::from_utf8(top.stdout).ok()?.trim()).ok()?;
    if top != std::fs::canonicalize(work_dir).ok()? {
        return None;
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(work_dir)
        .args(["ls-files", "-z"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let paths: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|p| String::from_utf8(p.to_vec()).ok())
        .collect();
    // A directory that is not itself a repository root still answers `git ls-files`
    // for an enclosing repository; an empty answer sends us to the walk instead.
    (!paths.is_empty()).then_some(paths)
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        let path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if ft.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                walk(&e.path(), &path, out);
            }
        } else if ft.is_file() {
            out.push(path);
        }
    }
}

pub fn inventory(work_dir: &Path) -> Inventory {
    inventory_capped(work_dir, MAX_PATHS)
}

pub(crate) fn inventory_capped(work_dir: &Path, cap: usize) -> Inventory {
    let mut paths = tracked_paths(work_dir).unwrap_or_else(|| {
        let mut v = Vec::new();
        walk(work_dir, "", &mut v);
        v
    });
    paths.retain(|p| !is_env_file(p.rsplit('/').next().unwrap_or(p)));
    paths.sort_by(|a, b| depth(a).cmp(&depth(b)).then_with(|| a.cmp(b)));
    let truncated = paths.len() > cap;
    paths.truncate(cap);
    let mut entries: Vec<Entry> = paths
        .into_iter()
        .filter_map(|p| {
            let full = work_dir.join(&p);
            let meta = std::fs::symlink_metadata(&full).ok()?;
            if !meta.file_type().is_file() {
                return None; // symlinks, submodules, deleted-but-tracked
            }
            let size = meta.len();
            let ext = p.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            let limit = if is_schema_like(&p) {
                MAX_SCHEMA_BYTES
            } else {
                MAX_READ_BYTES
            };
            let readable =
                size <= limit && !looks_binary(&full, BINARY_EXTS.contains(&ext.as_str()));
            Some(Entry {
                path: p,
                size,
                readable,
            })
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Inventory { entries, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    fn git_repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(d.path())
                .args(args)
                .status()
                .unwrap();
            assert!(ok.success());
        };
        run(&["init", "-q"]);
        d
    }

    fn add_all(d: &Path) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(d)
            .args(["add", "-A"])
            .status()
            .unwrap();
        assert!(ok.success());
    }

    #[test]
    fn lists_tracked_files_with_sizes_and_skips_untracked_and_env_files() {
        let d = git_repo();
        write(d.path(), "app/models/order.rb", b"class Order; end\n");
        write(d.path(), ".env", b"A=1\n");
        write(d.path(), "config/prod.env", b"A=1\n");
        write(d.path(), ".env.example", b"A=\n");
        add_all(d.path());
        write(d.path(), "untracked.rb", b"x");
        let inv = inventory(d.path());
        let paths: Vec<&str> = inv.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["app/models/order.rb"]);
        assert_eq!(inv.get("app/models/order.rb").unwrap().size, 17);
        assert!(inv.readable("app/models/order.rb"));
        assert!(!inv.readable("missing.rb"));
    }

    #[test]
    fn big_and_binary_files_are_listed_but_unreadable() {
        let d = git_repo();
        write(d.path(), "big.rb", &vec![b'a'; 300 * 1024]);
        write(d.path(), "logo.png", &[0x89, b'P', b'N', b'G', 0, 1, 2]);
        write(d.path(), "blob.dat", &[b'a', 0, b'b']);
        add_all(d.path());
        let inv = inventory(d.path());
        assert_eq!(inv.entries.len(), 3);
        assert!(inv.entries.iter().all(|e| !e.readable));
    }

    #[test]
    fn a_schema_like_file_is_readable_up_to_two_megabytes() {
        assert!(is_schema_like("db/schema.rb"));
        assert!(is_schema_like("db/structure.sql"));
        assert!(is_schema_like("prisma/schema.prisma"));
        assert!(!is_schema_like("app/models/order.rb"));
        let d = git_repo();
        write(d.path(), "db/schema.rb", &vec![b'a'; 1024 * 1024]);
        add_all(d.path());
        assert!(inventory(d.path()).readable("db/schema.rb"));
    }

    #[test]
    fn without_git_it_walks_the_tree_and_skips_vendor_dirs() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "src/main.rs", b"fn main() {}");
        write(d.path(), "node_modules/x/index.js", b"x");
        write(d.path(), "target/debug/a", b"x");
        write(d.path(), ".git/config", b"x");
        write(d.path(), ".env", b"A=1");
        let inv = inventory(d.path());
        let paths: Vec<&str> = inv.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["src/main.rs"]);
    }

    #[test]
    fn symlinks_are_not_listed() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rb", b"x");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", d.path().join("link.rb")).unwrap();
        let inv = inventory(d.path());
        assert_eq!(inv.entries.len(), 1);
    }

    #[test]
    fn over_the_cap_the_shallowest_paths_win_and_it_says_so() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "top.rb", b"x");
        write(d.path(), "a/b/c/deep1.rb", b"x");
        write(d.path(), "a/b/c/deep2.rb", b"x");
        let inv = inventory_capped(d.path(), 2);
        assert!(inv.truncated);
        assert_eq!(inv.entries.len(), 2);
        assert!(inv.get("top.rb").is_some());
    }

    #[test]
    fn render_cuts_the_deepest_paths_first_to_fit_a_budget() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "top.rb", b"x");
        write(d.path(), "a/b/c/deep.rb", b"x");
        let inv = inventory(d.path());
        let full = inv.render(10_000);
        assert!(full.contains("top.rb") && full.contains("a/b/c/deep.rb"));
        let tight = inv.render(full.len() - 1);
        assert!(tight.contains("top.rb") && !tight.contains("deep.rb"));
        assert!(tight.len() < full.len());
    }

    #[test]
    fn an_empty_directory_gives_an_empty_inventory() {
        let d = tempfile::tempdir().unwrap();
        let inv = inventory(d.path());
        assert!(inv.entries.is_empty() && !inv.truncated);
        assert_eq!(inv.render(100), "");
    }

    #[test]
    fn a_subdirectory_of_a_repo_is_walked_not_listed_through_the_repo() {
        let d = git_repo();
        write(d.path(), "outer.rb", b"x");
        write(d.path(), "sub/inner.rb", b"x");
        add_all(d.path());
        let inv = inventory(&d.path().join("sub"));
        let paths: Vec<&str> = inv.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["inner.rb"]);
    }
}
