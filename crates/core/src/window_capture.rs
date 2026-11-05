//! `loopx screenshot --window`: save a PNG of the front Google Chrome window, the
//! one the Claude in Chrome extension has just driven to a local page.
//!
//! The extension hands screenshots to the model only, never to disk, and the agent
//! is not given `osascript` or `screencapture` (both reach far beyond a browser), so
//! this fixed helper does the two macOS steps itself: it refuses unless the active
//! tab is a `localhost` page (a signed-in site the agent wandered to is never
//! captured), then `screencapture -l <window id>` saves just that window, even when
//! other windows cover it. Needs the Screen Recording permission for the app that
//! runs it, and Automation permission to ask Chrome for the tab's address.

use std::path::{Path, PathBuf};

/// The active tab's address, asked of Chrome by a fixed script.
const URL_SCRIPT: &str =
    "tell application \"Google Chrome\" to get URL of active tab of front window";

/// The front Chrome window's CoreGraphics window number (windows come front to
/// back; layer 0 skips menus and overlays, width skips tool windows).
const WINDOW_SCRIPT: &str = "ObjC.import('CoreGraphics');\
var list = ObjC.deepUnwrap(ObjC.castRefToObject($.CGWindowListCopyWindowInfo(17, 0)));\
var w = list.filter(function (x) { return x.kCGWindowOwnerName === 'Google Chrome' \
&& x.kCGWindowLayer === 0 && x.kCGWindowBounds.Width > 200; })[0];\
w ? String(w.kCGWindowNumber) : '';";

/// The largest PNG accepted, in bytes.
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;

const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Runs one program with arguments and returns its stdout.
pub type Runner<'a> = dyn Fn(&str, &[String]) -> Result<String, String> + 'a;

/// Whether `url` is an http(s) page on this machine (`localhost`, `127.0.0.1`,
/// `[::1]`), with no userinfo trick like `http://localhost@evil.test/`.
pub fn is_local_page(url: &str) -> bool {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Just the host of `url` for an error message: a tab's path and query can hold
/// private data the agent has no need to see.
fn host_of(url: &str) -> String {
    url.trim()
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .filter(|h| !h.is_empty() && !h.contains('@'))
        .unwrap_or("another page")
        .to_string()
}

/// `out` checked against the folder screenshots may go to: a `.png` inside
/// `allowed_dir`, which is created when missing. Returns the resolved path.
fn checked_out(out: &Path, allowed_dir: &Path) -> Result<PathBuf, String> {
    if out.extension().and_then(|e| e.to_str()) != Some("png") {
        return Err("--out must be a .png file".into());
    }
    std::fs::create_dir_all(allowed_dir).map_err(|e| format!("{}: {e}", allowed_dir.display()))?;
    let root = allowed_dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", allowed_dir.display()))?;
    let parent = out.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = parent.ok_or("--out needs a directory")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let parent = parent
        .canonicalize()
        .map_err(|e| format!("{}: {e}", parent.display()))?;
    if !parent.starts_with(&root) {
        return Err(format!(
            "--out must be inside {} (screenshots are kept there only)",
            root.display()
        ));
    }
    let name = out.file_name().ok_or("--out needs a file name")?;
    Ok(parent.join(name))
}

/// Save the front Chrome window as `out` (inside `allowed_dir`). `run` executes
/// the macOS programs, so tests can stand in for them.
pub fn capture_front_window(
    out: &Path,
    allowed_dir: &Path,
    run: &Runner<'_>,
) -> Result<PathBuf, String> {
    let out = checked_out(out, allowed_dir)?;
    let url = run("osascript", &["-e".into(), URL_SCRIPT.into()])
        .map_err(|e| format!("could not ask Chrome for the tab address: {e}"))?;
    if !is_local_page(&url) {
        return Err(format!(
            "the active Chrome tab is not a localhost page ({}); open the page with the \
             Claude in Chrome tools first",
            host_of(&url)
        ));
    }
    let id = run(
        "osascript",
        &[
            "-l".into(),
            "JavaScript".into(),
            "-e".into(),
            WINDOW_SCRIPT.into(),
        ],
    )
    .map_err(|e| format!("could not find the Chrome window: {e}"))?;
    let id = id.trim();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err("no Chrome window is on screen".into());
    }
    let _ = std::fs::remove_file(&out);
    run(
        "screencapture",
        &[
            "-x".into(),
            "-o".into(),
            "-l".into(),
            id.to_string(),
            out.display().to_string(),
        ],
    )
    .map_err(|e| format!("screencapture failed: {e}"))?;
    let bytes = std::fs::read(&out).map_err(|_| {
        "screencapture saved nothing (grant Screen Recording to LoopX in System Settings)"
            .to_string()
    })?;
    if bytes.len() as u64 > MAX_BYTES || !bytes.starts_with(&PNG_MAGIC) {
        let _ = std::fs::remove_file(&out);
        return Err("the capture is not a PNG within 5 MB".into());
    }
    Ok(out)
}

/// The real macOS programs.
pub fn system_runner(program: &str, args: &[String]) -> Result<String, String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{program}: {}", err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 9, 9];

    #[test]
    fn only_local_pages_count() {
        for ok in [
            "http://localhost:3000/orders",
            "https://127.0.0.1/",
            "http://[::1]:8080/x",
            "http://localhost",
        ] {
            assert!(is_local_page(ok), "{ok}");
        }
        for bad in [
            "https://app.example.com/",
            "http://localhost@evil.test/",
            "http://localhost.evil.test/",
            "file:///etc/passwd",
            "chrome://settings",
            "about:blank",
            "",
        ] {
            assert!(!is_local_page(bad), "{bad}");
        }
    }

    /// Records every call; answers the URL and window-id scripts, and writes the
    /// "screenshot" when `screencapture` runs.
    fn fake<'a>(
        url: &'a str,
        id: &'a str,
        png: &'a [u8],
        calls: &'a RefCell<Vec<String>>,
    ) -> impl Fn(&str, &[String]) -> Result<String, String> + 'a {
        move |program, args| {
            calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            match program {
                "osascript" if args[0] == "-e" => Ok(format!("{url}\n")),
                "osascript" => Ok(format!("{id}\n")),
                "screencapture" => {
                    std::fs::write(args.last().unwrap(), png).unwrap();
                    Ok(String::new())
                }
                other => Err(format!("unexpected {other}")),
            }
        }
    }

    #[test]
    fn saves_the_front_window_of_a_local_page() {
        let d = tempfile::tempdir().unwrap();
        let screens = d.path().join("outputs/screens");
        let out = screens.join("web-5/1-after.png");
        let calls = RefCell::new(Vec::new());
        let saved = capture_front_window(
            &out,
            &screens,
            &fake("http://localhost:3000/", "94", PNG, &calls),
        )
        .unwrap();
        assert!(saved.ends_with("web-5/1-after.png"));
        assert_eq!(std::fs::read(&saved).unwrap(), PNG);
        let calls = calls.borrow();
        assert_eq!(calls.len(), 3);
        assert!(
            calls[2].starts_with("screencapture -x -o -l 94 "),
            "{calls:?}"
        );
    }

    #[test]
    fn a_page_that_is_not_local_is_never_captured() {
        let d = tempfile::tempdir().unwrap();
        let screens = d.path().join("s");
        let calls = RefCell::new(Vec::new());
        let e = capture_front_window(
            &screens.join("a.png"),
            &screens,
            &fake("https://mail.example.com/inbox", "94", PNG, &calls),
        )
        .unwrap_err();
        assert!(e.contains("not a localhost page"), "{e}");
        assert!(
            e.contains("(mail.example.com)") && !e.contains("inbox"),
            "{e}"
        );
        assert!(
            calls
                .borrow()
                .iter()
                .all(|c| !c.starts_with("screencapture")),
            "{calls:?}"
        );
        assert!(!screens.join("a.png").exists());
    }

    #[test]
    fn the_output_stays_inside_the_screens_folder_and_is_a_png() {
        let d = tempfile::tempdir().unwrap();
        let screens = d.path().join("s");
        let calls = RefCell::new(Vec::new());
        let f = fake("http://localhost/", "1", PNG, &calls);
        for bad in [
            d.path().join("elsewhere/a.png"),
            screens.join("../a.png"),
            screens.join("a.txt"),
            PathBuf::from("a.png"),
        ] {
            assert!(capture_front_window(&bad, &screens, &f).is_err(), "{bad:?}");
        }
        assert!(calls.borrow().is_empty(), "nothing ran: {calls:?}");
    }

    #[test]
    fn no_window_or_a_bad_capture_is_an_error_and_leaves_no_file() {
        let d = tempfile::tempdir().unwrap();
        let screens = d.path().join("s");
        let out = screens.join("a.png");
        let calls = RefCell::new(Vec::new());
        let e = capture_front_window(&out, &screens, &fake("http://localhost/", "", PNG, &calls))
            .unwrap_err();
        assert!(e.contains("no Chrome window"), "{e}");
        let e = capture_front_window(
            &out,
            &screens,
            &fake("http://localhost/", "7", b"not a png", &calls),
        )
        .unwrap_err();
        assert!(e.contains("not a PNG"), "{e}");
        assert!(!out.exists());
    }
}
