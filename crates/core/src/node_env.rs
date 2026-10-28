//! Which Node.js a project's commands run under.
//!
//! The loop runs from launchd with whatever `node` is first on `PATH` (often
//! the nvm default), which need not be the version a project needs. Under a
//! too-old Node, `npm ci` silently skips optional native bindings whose
//! `engines` it does not satisfy (`@oxlint/binding-*`, `@rolldown/binding-*`)
//! and the lint/test tools then die at startup with "Cannot find native
//! binding". So for a work directory this picks an installed Node the project
//! accepts: a `.nvmrc`/`.node-version`/`.tool-versions` pin first, otherwise the
//! `engines.node` ranges of its `package.json` and every package in its
//! `package-lock.json`, keeping the current Node whenever it already fits.
//!
//! The agent reaches this through `node`/`npm`/`npx`/... shims on its `PATH`
//! (`loopx _node-shim`), the loop's own verification re-run through
//! [`path_for`].

use serde_json::Value;
use std::path::{Path, PathBuf};

/// The executables the shims stand in for.
pub const SHIMMED: &[&str] = &["node", "npm", "npx", "corepack", "pnpm", "yarn"];

pub type Version = (u64, u64, u64);

/// Where Node version managers keep their installs.
#[derive(Debug, Clone)]
pub struct Toolchains {
    pub home: PathBuf,
    pub nvm_dir: Option<PathBuf>,
}

/// One installed Node: its version and the `bin` directory holding `node`.
#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    pub version: Version,
    pub bin: PathBuf,
}

/// An npm semver range: any one of the alternatives (`||`) satisfies it.
#[derive(Debug, Clone, PartialEq)]
pub struct Range(Vec<Vec<(Op, Version)>>);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
}

/// What a directory asks for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wanted {
    /// A version-file pin, as written (`22`, `v22.12.0`, `lts/*`).
    pub pin: Option<String>,
    /// The project's own `engines.node`.
    pub own: Option<Range>,
    /// `engines.node` of every package in the lockfile installed on this platform.
    pub deps: Vec<Range>,
}

impl Toolchains {
    pub fn from_env() -> Self {
        Self {
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
            nvm_dir: std::env::var_os("NVM_DIR").map(PathBuf::from),
        }
    }

    /// Every Node the version managers have installed, newest first.
    pub fn installed(&self) -> Vec<Installed> {
        let h = &self.home;
        let nvm = self.nvm_dir.clone().unwrap_or_else(|| h.join(".nvm"));
        // (directory of per-version installs, path from an install to its bin)
        let roots = [
            (nvm.join("versions/node"), "bin"),
            (h.join(".local/share/fnm/node-versions"), "installation/bin"),
            (
                h.join("Library/Application Support/fnm/node-versions"),
                "installation/bin",
            ),
            (h.join(".local/share/mise/installs/node"), "bin"),
            (h.join(".asdf/installs/nodejs"), "bin"),
            (h.join(".volta/tools/image/node"), "bin"),
        ];
        let mut found: Vec<Installed> = Vec::new();
        for (root, bin) in roots {
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            let mut here: Vec<Installed> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let version = parse_version(&e.file_name().to_string_lossy())?;
                    let bin = e.path().join(bin);
                    crate::doctor::find_on_path("node", &bin.to_string_lossy())?;
                    Some(Installed { version, bin })
                })
                .collect();
            here.sort_by_key(|i| i.bin.clone());
            for i in here {
                if !found.iter().any(|f| f.version == i.version) {
                    found.push(i);
                }
            }
        }
        found.sort_by(|a, b| b.version.cmp(&a.version));
        found
    }
}

/// `v22.12.0`, `22.12.0` or `22.1.0-rc.1`; partial versions are not versions.
pub fn parse_version(s: &str) -> Option<Version> {
    match partial(s.trim())? {
        (Some(major), Some(minor), Some(patch)) => Some((major, minor, patch)),
        _ => None,
    }
}

type Partial = (Option<u64>, Option<u64>, Option<u64>);

/// `22`, `22.x`, `v22.12.0`, `*`: missing or wildcard parts are `None`.
fn partial(s: &str) -> Option<Partial> {
    let s = s.trim_start_matches(['v', 'V', '=']);
    let s = s.split(['-', '+']).next().unwrap_or("");
    if s.is_empty() {
        return Some((None, None, None));
    }
    let mut parts = [None; 3];
    for (i, part) in s.split('.').enumerate() {
        if i >= 3 {
            return None;
        }
        parts[i] = match part {
            "x" | "X" | "*" => None,
            n => Some(n.parse().ok()?),
        };
    }
    // Nothing after a wildcard counts: `22.x.5` is `22.x`.
    if let Some(i) = parts.iter().position(Option::is_none) {
        parts[i..].fill(None);
    }
    Some((parts[0], parts[1], parts[2]))
}

/// The comparators for one npm range token such as `^20.19.0` or `<=20`.
fn comparators(op: &str, p: Partial) -> Vec<(Op, Version)> {
    let (Some(major), minor, patch) = p else {
        return Vec::new();
    };
    let lo = (major, minor.unwrap_or(0), patch.unwrap_or(0));
    // The first version past a partial one: `20` -> 21.0.0, `20.1` -> 20.2.0.
    let next = match (minor, patch) {
        (None, _) => Some((major + 1, 0, 0)),
        (Some(m), None) => Some((major, m + 1, 0)),
        _ => None,
    };
    match (op, next) {
        ("" | "=", None) => vec![(Op::Ge, lo), (Op::Le, lo)],
        ("" | "=", Some(n)) => vec![(Op::Ge, lo), (Op::Lt, n)],
        (">=", _) => vec![(Op::Ge, lo)],
        (">", None) => vec![(Op::Gt, lo)],
        (">", Some(n)) => vec![(Op::Ge, n)],
        ("<", _) => vec![(Op::Lt, lo)],
        ("<=", None) => vec![(Op::Le, lo)],
        ("<=", Some(n)) => vec![(Op::Lt, n)],
        ("~", _) => match minor {
            None => vec![(Op::Ge, lo), (Op::Lt, (major + 1, 0, 0))],
            Some(m) => vec![(Op::Ge, lo), (Op::Lt, (major, m + 1, 0))],
        },
        // `^`: up to the next change of the first non-zero part.
        _ => {
            let hi = match (major, minor, patch) {
                (0, None, _) => (1, 0, 0),
                (0, Some(0), None) => (0, 1, 0),
                (0, Some(0), Some(p)) => (0, 0, p + 1),
                (0, Some(m), _) => (0, m + 1, 0),
                (m, _, _) => (m + 1, 0, 0),
            };
            vec![(Op::Ge, lo), (Op::Lt, hi)]
        }
    }
}

const OPS: &[&str] = &[">=", "<=", "~>", ">", "<", "=", "^", "~"];

fn split_op(token: &str) -> (&str, &str) {
    for op in OPS {
        if let Some(rest) = token.strip_prefix(op) {
            return (if *op == "~>" { "~" } else { op }, rest);
        }
    }
    ("", token)
}

impl Range {
    /// An npm range; `None` for anything it cannot read (`lts/iron`).
    pub fn parse(s: &str) -> Option<Range> {
        let mut alts = Vec::new();
        for alt in s.split("||") {
            let alt = alt.trim();
            let mut all = Vec::new();
            if let Some((a, b)) = alt.split_once(" - ") {
                all.extend(comparators(">=", partial(a.trim())?));
                all.extend(comparators("<=", partial(b.trim())?));
            } else {
                let mut tokens = alt.split_whitespace();
                while let Some(token) = tokens.next() {
                    // `>= 14.17`: an operator on its own takes the next word.
                    let joined;
                    let token = if OPS.contains(&token) {
                        joined = format!("{token}{}", tokens.next()?);
                        joined.as_str()
                    } else {
                        token
                    };
                    let (op, rest) = split_op(token);
                    if rest.is_empty()
                        || !rest.starts_with(|c: char| c.is_ascii_digit() || "vVxX*".contains(c))
                    {
                        return None;
                    }
                    all.extend(comparators(op, partial(rest)?));
                }
            }
            alts.push(all);
        }
        Some(Range(alts))
    }

    pub fn satisfies(&self, v: Version) -> bool {
        self.0.iter().any(|alt| {
            alt.iter().all(|&(op, bound)| match op {
                Op::Ge => v >= bound,
                Op::Gt => v > bound,
                Op::Le => v <= bound,
                Op::Lt => v < bound,
            })
        })
    }
}

/// The version-file pin in `dir` itself, if any.
fn pin_in(dir: &Path) -> Option<String> {
    for name in [".nvmrc", ".node-version"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            let line = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'));
            if let Some(line) = line {
                return Some(line.to_string());
            }
        }
    }
    let text = std::fs::read_to_string(dir.join(".tool-versions")).ok()?;
    text.lines().find_map(|l| {
        let mut words = l.split_whitespace();
        matches!(words.next(), Some("nodejs" | "node"))
            .then(|| words.next().map(str::to_string))
            .flatten()
    })
}

/// Whether an npm `os`/`cpu` list admits `me` (`["darwin"]`, `["!win32"]`).
fn platform_ok(list: Option<&Value>, me: &str) -> bool {
    let Some(list) = list.and_then(Value::as_array) else {
        return true;
    };
    let entries: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
    if entries.contains(&me) {
        return true;
    }
    let negated = format!("!{me}");
    !entries.is_empty()
        && entries.iter().all(|e| e.starts_with('!'))
        && !entries.contains(&negated.as_str())
}

fn engines_node(package: &Value) -> Option<Range> {
    Range::parse(package.get("engines")?.get("node")?.as_str()?)
}

/// What `dir` asks for: the nearest version file, and the nearest
/// `package.json` with its lockfile.
pub fn wanted(dir: &Path) -> Wanted {
    let mut wanted = Wanted {
        pin: dir.ancestors().find_map(pin_in),
        ..Wanted::default()
    };
    let read = |p: PathBuf| -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
    };
    let Some(project) = dir.ancestors().find(|d| d.join("package.json").is_file()) else {
        return wanted;
    };
    wanted.own = read(project.join("package.json"))
        .as_ref()
        .and_then(engines_node);
    let lock = read(project.join("package-lock.json"))
        .or_else(|| read(project.join("npm-shrinkwrap.json")));
    if let Some(packages) = lock.as_ref().and_then(|l| l.get("packages")?.as_object()) {
        for (key, package) in packages {
            if key.is_empty()
                || !platform_ok(package.get("os"), npm_os())
                || !platform_ok(package.get("cpu"), npm_cpu())
            {
                continue;
            }
            if let Some(range) = engines_node(package) {
                if !wanted.deps.contains(&range) {
                    wanted.deps.push(range);
                }
            }
        }
    }
    wanted
}

/// The Node major an nvm `lts/<codename>` alias names.
fn lts_major(codename: &str) -> Option<u64> {
    let names = [
        "argon", "boron", "carbon", "dubnium", "erbium", "fermium", "gallium", "hydrogen", "iron",
        "jod", "krypton",
    ];
    let i = names.iter().position(|n| *n == codename)?;
    Some(4 + 2 * i as u64)
}

type Accepts = Box<dyn Fn(Version) -> bool>;

/// The versions a pin accepts, and whether it means the newest of them
/// (`node`, `lts/*`) rather than any; `None` for a pin it cannot read.
fn pin_accepts(pin: &str) -> Option<(Accepts, bool)> {
    let pin = pin.trim().to_ascii_lowercase();
    match pin.as_str() {
        "node" | "stable" | "latest" | "current" => Some((Box::new(|_| true), true)),
        "lts/*" | "lts" => Some((Box::new(|v: Version| v.0 % 2 == 0), true)),
        _ => {
            if let Some(major) = pin.strip_prefix("lts/").and_then(lts_major) {
                return Some((Box::new(move |v: Version| v.0 == major), false));
            }
            let range = Range::parse(&pin)?;
            Some((Box::new(move |v| range.satisfies(v)), false))
        }
    }
}

/// The `bin` directory to put first on `PATH`, or `None` to keep `current`.
pub fn choose(
    wanted: &Wanted,
    installed: &[Installed],
    current: Option<Version>,
) -> Option<PathBuf> {
    let keeps = |ok: &dyn Fn(Version) -> bool| current.is_some_and(ok);
    let newest = |ok: &dyn Fn(Version) -> bool| {
        installed
            .iter()
            .filter(|i| ok(i.version))
            .max_by_key(|i| i.version)
            .map(|i| i.bin.clone())
    };
    if let Some((accepts, latest)) = wanted.pin.as_deref().and_then(pin_accepts) {
        if latest {
            let top = installed
                .iter()
                .filter(|i| accepts(i.version))
                .max_by_key(|i| i.version)?;
            return (current != Some(top.version)).then(|| top.bin.clone());
        }
        return if keeps(&*accepts) {
            None
        } else {
            newest(&*accepts)
        };
    }
    if wanted.own.is_none() && wanted.deps.is_empty() {
        return None;
    }
    let fits_all = |v: Version| {
        wanted
            .own
            .iter()
            .chain(&wanted.deps)
            .all(|r| r.satisfies(v))
    };
    if keeps(&fits_all) {
        return None;
    }
    if let Some(bin) = newest(&fits_all) {
        return Some(bin);
    }
    // No install fits every dependency: at least honour the project itself.
    let own = wanted.own.as_ref()?;
    let fits_own = |v: Version| own.satisfies(v);
    if keeps(&fits_own) {
        None
    } else {
        newest(&fits_own)
    }
}

/// The version of the first `node` on `path`.
fn current_version(path: &str) -> Option<Version> {
    let node = crate::doctor::find_on_path("node", path)?;
    let out = std::process::Command::new(node).arg("-v").output().ok()?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// `path` with the Node `dir` needs put first (unchanged when the current one fits).
pub fn path_for(dir: &Path, path: &str, tc: &Toolchains) -> String {
    let wanted = wanted(dir);
    if wanted == Wanted::default() {
        return path.to_string();
    }
    match choose(&wanted, &tc.installed(), current_version(path)) {
        Some(bin) => format!("{}:{path}", bin.display()),
        None => path.to_string(),
    }
}

fn sh_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
}

/// Write the `node`/`npm`/... shims into `shim_dir`, each running `exe _node-shim`.
pub fn write_shims(shim_dir: &Path, exe: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(shim_dir)?;
    for tool in SHIMMED {
        let text = format!(
            "#!/bin/sh\nexec {} _node-shim {} {tool} \"$@\"\n",
            sh_quote(exe),
            sh_quote(shim_dir)
        );
        let target = shim_dir.join(tool);
        if std::fs::read_to_string(&target).ok().as_deref() == Some(text.as_str()) {
            continue;
        }
        // Rename into place: another run may be executing this shim right now.
        let tmp = shim_dir.join(format!(".{tool}.{}", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, &target)?;
    }
    Ok(())
}

/// The real program a shim for `tool` should run from `cwd`, and the `PATH`
/// to run it with (the shims removed, the chosen Node first).
pub fn shim_target(
    shim_dir: &Path,
    tool: &str,
    cwd: &Path,
    path: &str,
    tc: &Toolchains,
) -> Option<(PathBuf, String)> {
    let rest: Vec<&str> = path
        .split(':')
        .filter(|d| !d.is_empty() && Path::new(d) != shim_dir)
        .collect();
    let path = path_for(cwd, &rest.join(":"), tc);
    let program = crate::doctor::find_on_path(tool, &path)?;
    Some((program, path))
}

/// `loopx _node-shim <shim-dir> <tool> [args...]`: run `tool` from the Node the
/// current directory needs. Only returns (an exit code) when it cannot.
pub fn shim_main(args: &[String]) -> i32 {
    use std::os::unix::process::CommandExt;
    let [shim_dir, tool, rest @ ..] = args else {
        eprintln!("usage: loopx _node-shim <shim-dir> <tool> [args...]");
        return 2;
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let path = std::env::var("PATH").unwrap_or_default();
    let tc = Toolchains::from_env();
    let Some((program, path)) = shim_target(Path::new(shim_dir), tool, &cwd, &path, &tc) else {
        eprintln!("{tool}: command not found");
        return 127;
    };
    let err = std::process::Command::new(&program)
        .args(rest)
        .env("PATH", path)
        .exec();
    eprintln!("cannot run {}: {err}", program.display());
    126
}

fn npm_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        os => os,
    }
}

fn npm_cpu() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        "x86" => "ia32",
        arch => arch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn v(s: &str) -> Version {
        parse_version(s).unwrap()
    }

    fn sat(range: &str, version: &str) -> bool {
        Range::parse(range).unwrap().satisfies(v(version))
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A fake `node` that prints `version` for `-v`.
    fn fake_node(bin: &Path, version: &str) {
        let node = bin.join("node");
        write(&node, &format!("#!/bin/sh\necho {version}\n"));
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn installed(list: &[&str]) -> Vec<Installed> {
        list.iter()
            .map(|s| Installed {
                version: v(s),
                bin: PathBuf::from(format!("/n/{s}/bin")),
            })
            .collect()
    }

    #[test]
    fn versions_parse_with_or_without_v_and_partials() {
        assert_eq!(parse_version("v22.12.0"), Some((22, 12, 0)));
        assert_eq!(parse_version("20.15.0\n"), Some((20, 15, 0)));
        assert_eq!(parse_version("22.1.0-rc.1"), Some((22, 1, 0)));
        assert_eq!(parse_version("lts/iron"), None);
    }

    #[test]
    fn npm_ranges() {
        // The range that broke encor-client-portal under Node 20.15.
        assert!(!sat("^20.19.0 || >=22.12.0", "20.15.0"));
        assert!(sat("^20.19.0 || >=22.12.0", "20.19.1"));
        assert!(!sat("^20.19.0 || >=22.12.0", "21.0.0"));
        assert!(sat("^20.19.0 || >=22.12.0", "22.18.0"));
        assert!(sat(">=18", "18.0.0"));
        assert!(sat(">= 14.17 < 21", "20.9.0"));
        assert!(!sat(">= 14.17 < 21", "21.0.0"));
        assert!(!sat(">14.17", "14.17.9"));
        assert!(sat("<=20", "20.9.0"));
        assert!(sat("~20.10.0", "20.10.5"));
        assert!(!sat("~20.10.0", "20.11.0"));
        assert!(sat("20.x", "20.1.0"));
        assert!(sat("20", "20.1.0"));
        assert!(!sat("20.1.0", "20.1.1"));
        assert!(sat("18 - 20", "20.5.0"));
        assert!(!sat("18 - 20", "21.0.0"));
        assert!(sat("*", "4.0.0"));
        assert!(sat("^0.2.3", "0.2.9"));
        assert!(!sat("^0.2.3", "0.3.0"));
        assert_eq!(Range::parse("lts/iron"), None);
    }

    #[test]
    fn a_version_file_pins_from_any_parent() {
        let d = tempfile::tempdir().unwrap();
        write(&d.path().join(".nvmrc"), "v22\n");
        write(
            &d.path().join("web/package.json"),
            r#"{"engines":{"node":">=18"}}"#,
        );
        let w = wanted(&d.path().join("web/src"));
        assert_eq!(w.pin.as_deref(), Some("v22"));

        let d = tempfile::tempdir().unwrap();
        write(
            &d.path().join(".tool-versions"),
            "ruby 3.3.0\nnodejs 20.19.0\n",
        );
        assert_eq!(wanted(d.path()).pin.as_deref(), Some("20.19.0"));
    }

    #[test]
    fn engines_come_from_package_json_and_this_platforms_lock_entries() {
        let d = tempfile::tempdir().unwrap();
        let web = d.path().join("web");
        write(&web.join("package.json"), r#"{"engines":{"node":">=18"}}"#);
        let (os, cpu) = (npm_os(), npm_cpu());
        write(
            &web.join("package-lock.json"),
            &serde_json::json!({"lockfileVersion": 3, "packages": {
                "": {"engines": {"node": "ignored, it is package.json"}},
                "node_modules/a": {"engines": {"node": "^20.19.0 || >=22.12.0"}},
                "node_modules/b": {},
                "node_modules/here": {"os": [os], "cpu": [cpu], "engines": {"node": ">=22"}},
                "node_modules/elsewhere": {"os": ["plan9"], "engines": {"node": ">=99"}},
                "node_modules/not-here": {"os": [format!("!{os}")], "engines": {"node": ">=98"}},
            }})
            .to_string(),
        );
        let w = wanted(&web);
        assert_eq!(w.pin, None);
        assert_eq!(w.own, Range::parse(">=18"));
        assert_eq!(
            w.deps,
            vec![
                Range::parse("^20.19.0 || >=22.12.0").unwrap(),
                Range::parse(">=22").unwrap()
            ]
        );
        assert_eq!(wanted(d.path()), Wanted::default());
    }

    #[test]
    fn keeps_the_current_node_when_it_fits_else_the_newest_that_does() {
        let all = installed(&["22.18.0", "22.12.0", "20.15.0", "18.1.0"]);
        let w = Wanted {
            deps: vec![Range::parse("^20.19.0 || >=22.12.0").unwrap()],
            ..Wanted::default()
        };
        assert_eq!(
            choose(&w, &all, Some(v("20.15.0"))),
            Some(PathBuf::from("/n/22.18.0/bin"))
        );
        assert_eq!(choose(&w, &all, Some(v("22.12.0"))), None);
        assert_eq!(
            choose(&w, &all, None),
            Some(PathBuf::from("/n/22.18.0/bin"))
        );
        // Nothing reported: keep whatever is there.
        assert_eq!(choose(&Wanted::default(), &all, Some(v("20.15.0"))), None);
    }

    #[test]
    fn the_projects_own_engines_win_when_no_install_fits_every_dependency() {
        let all = installed(&["22.18.0", "20.15.0", "18.1.0"]);
        let w = Wanted {
            own: Range::parse("<21"),
            deps: vec![Range::parse(">=22").unwrap()],
            ..Wanted::default()
        };
        assert_eq!(choose(&w, &all, Some(v("20.15.0"))), None);
        assert_eq!(
            choose(&w, &all, Some(v("22.18.0"))),
            Some(PathBuf::from("/n/20.15.0/bin"))
        );
    }

    #[test]
    fn a_pin_picks_the_newest_matching_install() {
        let all = installed(&["23.1.0", "22.18.0", "22.12.0", "20.15.0"]);
        let pin = |p: &str| Wanted {
            pin: Some(p.into()),
            // A pin overrides what the lockfile says.
            deps: vec![Range::parse(">=99").unwrap()],
            ..Wanted::default()
        };
        let cur = Some(v("20.15.0"));
        assert_eq!(
            choose(&pin("v22"), &all, cur),
            Some(PathBuf::from("/n/22.18.0/bin"))
        );
        assert_eq!(
            choose(&pin("22.12.0"), &all, cur),
            Some(PathBuf::from("/n/22.12.0/bin"))
        );
        assert_eq!(choose(&pin("20"), &all, cur), None);
        assert_eq!(
            choose(&pin("node"), &all, cur),
            Some(PathBuf::from("/n/23.1.0/bin"))
        );
        assert_eq!(
            choose(&pin("lts/*"), &all, cur),
            Some(PathBuf::from("/n/22.18.0/bin"))
        );
        // Not installed: keep the current one rather than guess.
        assert_eq!(choose(&pin("24"), &all, cur), None);
    }

    #[test]
    fn finds_installs_of_every_version_manager() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        fake_node(&h.join(".nvm/versions/node/v20.15.0/bin"), "v20.15.0");
        fake_node(&h.join(".nvm/versions/node/v22.18.0/bin"), "v22.18.0");
        fake_node(
            &h.join(".local/share/mise/installs/node/22.12.0/bin"),
            "v22.12.0",
        );
        fake_node(&h.join(".asdf/installs/nodejs/18.1.0/bin"), "v18.1.0");
        fake_node(
            &h.join(".local/share/fnm/node-versions/v21.0.0/installation/bin"),
            "v21.0.0",
        );
        fake_node(&h.join(".volta/tools/image/node/19.9.0/bin"), "v19.9.0");
        // A directory without a `node` binary is not an install.
        std::fs::create_dir_all(h.join(".nvm/versions/node/v23.0.0/bin")).unwrap();
        let tc = Toolchains {
            home: h.to_path_buf(),
            nvm_dir: None,
        };
        let got: Vec<Version> = tc.installed().iter().map(|i| i.version).collect();
        assert_eq!(
            got,
            vec![
                v("22.18.0"),
                v("22.12.0"),
                v("21.0.0"),
                v("20.15.0"),
                v("19.9.0"),
                v("18.1.0")
            ]
        );
        assert_eq!(
            tc.installed()[0].bin,
            h.join(".nvm/versions/node/v22.18.0/bin")
        );
    }

    /// A HOME with nvm's 20.15 (also on PATH) and 22.18, and a project whose
    /// lockfile needs ^20.19 || >=22.12 — encor-client-portal's situation.
    fn portal_setup() -> (tempfile::TempDir, Toolchains, PathBuf, String) {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        let old = home.join(".nvm/versions/node/v20.15.0/bin");
        let new = home.join(".nvm/versions/node/v22.18.0/bin");
        fake_node(&old, "v20.15.0");
        fake_node(&new, "v22.18.0");
        let web = d.path().join("wt/web");
        write(&web.join("package.json"), "{}");
        write(
            &web.join("package-lock.json"),
            r#"{"packages":{"node_modules/oxlint":{"engines":{"node":"^20.19.0 || >=22.12.0"}}}}"#,
        );
        let tc = Toolchains {
            home,
            nvm_dir: None,
        };
        let path = format!("{}:/usr/bin:/bin", old.display());
        (d, tc, web, path)
    }

    #[test]
    fn path_for_puts_the_needed_node_first_and_leaves_fitting_ones_alone() {
        let (d, tc, web, path) = portal_setup();
        let new = tc.home.join(".nvm/versions/node/v22.18.0/bin");
        assert_eq!(
            path_for(&web, &path, &tc),
            format!("{}:{path}", new.display())
        );
        // No package.json: nothing to go on, PATH unchanged.
        assert_eq!(path_for(d.path(), &path, &tc), path);
        // Already first on PATH: unchanged.
        let fits = format!("{}:/usr/bin", new.display());
        assert_eq!(path_for(&web, &fits, &tc), fits);
    }

    #[test]
    fn shims_run_the_chosen_nodes_tool_without_the_shims_on_path() {
        let (d, tc, web, path) = portal_setup();
        let shims = d.path().join("shims");
        write_shims(&shims, Path::new("/opt/loop x/loopx")).unwrap();
        let npm = std::fs::read_to_string(shims.join("npm")).unwrap();
        assert_eq!(
            npm,
            format!(
                "#!/bin/sh\nexec '/opt/loop x/loopx' _node-shim '{}' npm \"$@\"\n",
                shims.display()
            )
        );
        let mode = std::fs::metadata(shims.join("node"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);

        let new = tc.home.join(".nvm/versions/node/v22.18.0/bin");
        let shimmed = format!("{}:{path}", shims.display());
        assert_eq!(
            shim_target(&shims, "node", &web, &shimmed, &tc),
            Some((new.join("node"), format!("{}:{path}", new.display())))
        );
        // A tool the chosen Node lacks falls through to the rest of PATH, never a shim.
        let (program, _) = shim_target(&shims, "sh", &web, &shimmed, &tc).unwrap();
        assert_eq!(program, PathBuf::from("/bin/sh"));
        assert_eq!(shim_target(&shims, "yarn", &web, &shimmed, &tc), None);
    }
}
