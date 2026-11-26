//! Connectors -> MCP servers (`/connectors?view=mcp`): the per-machine registry
//! (`~/.loopx/mcp.json`), each server's install state, secrets and last test, and
//! the Test / Add Playwright / Remove actions. Never shows a secret value.

use super::loops_common::flash_html;
use crate::ui::icons;
use crate::ui::pyfmt::escape as h;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::state::UiState;
use loopx_core::keychain::SecretStore;
use loopx_core::loop_definition::LoopDefinition;
use loopx_core::mcp_probe::{self, SavedResult};
use loopx_core::mcp_registry::{self as reg, Launch, ServerSpec};
use loopx_core::mcp_run::Env;
use loopx_core::node_env::Toolchains;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

const BACK: &str = "/connectors?view=mcp";
const SHOWN_TOOLS: usize = 12;
const MESSAGE_CHARS: usize = 400;

pub fn register(r: &mut Registry) {
    r.post("/connectors/mcp/test", test);
    r.post("/connectors/mcp/add-playwright", add_playwright);
    r.post("/connectors/mcp/remove", remove);
}

fn store(state: &UiState) -> Arc<dyn SecretStore> {
    match &state.mcp_store {
        Some(s) => s.clone(),
        None => match loopx_core::keychain::open_store(reg::SERVICE_MCP) {
            Ok(s) => Arc::from(s),
            Err(_) => Arc::new(loopx_core::keychain::MemoryStore::new(reg::SERVICE_MCP)),
        },
    }
}

fn env(state: &UiState) -> Env {
    let (toolchains, launch_path) = state.mcp_toolchains.clone().unwrap_or_else(|| {
        (
            Toolchains::from_env(),
            loopx_core::runner_util::cli_launch_path(),
        )
    });
    Env {
        registry: reg::path(&state.home_dir),
        secrets: store(state),
        toolchains,
        launch_path,
        tmp_root: std::env::temp_dir(),
        launcher: std::env::current_exe().unwrap_or_default(),
        launch_keys: Arc::new(loopx_core::keychain::MemoryStore::new(
            loopx_core::mcp_run::LAUNCH_KEY_SERVICE,
        )),
    }
}

fn chrome(state: &UiState) -> Option<PathBuf> {
    match &state.mcp_chrome {
        Some(pinned) => pinned.clone(),
        None => loopx_core::linkedin_cdp::find_chrome(),
    }
}

fn results_path(state: &UiState) -> PathBuf {
    mcp_probe::results_path(&state.repo_root)
}

/// Loop display names whose `loop.yaml` declares `name`, plus Fix now when
/// the issue loop does (Fix now reads that list).
fn used_by(state: &UiState) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let issue = state
        .repo_root
        .join(loopx_core::issue_runner::DEFINITION_RELATIVE_PATH);
    let mut files: Vec<PathBuf> = std::fs::read_dir(state.repo_root.join("loops"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("loop.yaml"))
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    for file in files {
        let Ok(def) = LoopDefinition::from_yaml(&file) else {
            continue;
        };
        for name in &def.mcp_servers {
            let list = out.entry(name.clone()).or_default();
            list.push(loopx_core::loops_config::display_name(&def.name));
            if file == issue {
                list.push("Fix now".to_string());
            }
        }
    }
    out
}

fn pill(class: &str, text: &str) -> String {
    format!("<span class='pill {class}'>{}</span>", h(text))
}

fn install_block(ctx: &UiCtx, cmd: &str) -> String {
    format!(
        "<pre class='mcp-install'><code>{}</code></pre><p class='mcp-hint'>{}</p>",
        h(cmd),
        h(&ctx.t("Run this once in Terminal, then press Test."))
    )
}

/// The install row: installed (with Node), or what is missing and how to fix it.
fn install_html(ctx: &UiCtx, spec: &ServerSpec, env: &Env) -> String {
    match &spec.launch {
        Launch::Command(p) => {
            let ok = std::fs::metadata(p).is_ok_and(|m| {
                use std::os::unix::fs::PermissionsExt;
                m.is_file() && m.permissions().mode() & 0o111 != 0
            });
            let state = if ok {
                String::new()
            } else {
                pill("pill-red", &ctx.t("The command is not an executable file"))
            };
            format!("<div class='pill-row'><code>{}</code>{state}</div>", h(&p.to_string_lossy()))
        }
        Launch::NpmGlobal {
            package,
            version,
            bin,
        } => match reg::resolve_npm_global(package, version, bin, &env.toolchains, &env.launch_path) {
            Ok(r) => format!(
                "<div class='pill-row'>{}{}</div>",
                pill("pill-green", &ctx.tf("Installed {version}", &[("version", &r.version)])),
                pill("pill-grey", &ctx.tf("Node {version}", &[("version", &r.node_version)]))
            ),
            Err(reg::Missing::NotInstalled { install }) => format!(
                "<div class='pill-row'>{}</div>{}",
                pill("pill-red", &ctx.t("Not installed")),
                install_block(ctx, &install)
            ),
            Err(reg::Missing::WrongVersion { found, install, .. }) => format!(
                "<div class='pill-row'>{}</div>{}",
                pill(
                    "pill-red",
                    &ctx.tf(
                        "Wrong version: found {found}, needs {version}",
                        &[("found", &found), ("version", version)]
                    )
                ),
                install_block(ctx, &install)
            ),
            Err(reg::Missing::NoNode { .. }) => format!(
                "<div class='pill-row'>{}</div>",
                pill("pill-red", &ctx.t("Node 18 or newer was not found next to the install"))
            ),
        },
    }
}

fn result_html(ctx: &UiCtx, result: Option<&SavedResult>) -> String {
    match result {
        None => pill("pill-grey", &ctx.t("Never tested")),
        Some(r) if r.ok => {
            let n = r.tools.len().to_string();
            let mut out = pill("pill-green", &ctx.tf("Test passed: {n} tools", &[("n", &n)]));
            for t in r.tools.iter().take(SHOWN_TOOLS) {
                out.push_str(&format!("<code class='mcp-tool'>{}</code>", h(t)));
            }
            if r.tools.len() > SHOWN_TOOLS {
                let more = (r.tools.len() - SHOWN_TOOLS).to_string();
                out.push_str(&pill("pill-grey", &ctx.tf("+{n} more", &[("n", &more)])));
            }
            out
        }
        Some(r) => pill(
            "pill-red",
            &ctx.tf(
                "Test failed: {message}",
                &[("message", &loopx_core::runner_util::head(&r.message, MESSAGE_CHARS))],
            ),
        ),
    }
}

fn action_form(ctx: &UiCtx, action: &str, name: &str, class: &str, icon: &str, label: &str, confirm: Option<&str>) -> String {
    let confirm = confirm.map_or_else(String::new, |c| format!(" data-confirm=\"{}\"", h(c)));
    format!(
        "<form method='post' action='{action}'>{}<input type='hidden' name='name' value=\"{}\">\
<button type='submit' class='btn {class}'{confirm}>{} {}</button></form>",
        ctx.csrf_input(),
        h(name),
        icons::icon(icon),
        h(label)
    )
}

fn server_card(
    ctx: &UiCtx,
    name: &str,
    spec: &Result<ServerSpec, String>,
    env: &Env,
    stored: &[String],
    result: Option<&SavedResult>,
    used: Option<&Vec<String>>,
) -> String {
    let remove = action_form(
        ctx,
        "/connectors/mcp/remove",
        name,
        "btn-warning",
        "delete",
        &ctx.t("Remove"),
        Some(&ctx.tf("Remove {name}?", &[("name", name)])),
    );
    let header = format!(
        "<div class='section-header'>{}<h2>{}</h2></div>",
        icons::icon("extension"),
        h(name)
    );
    let spec = match spec {
        Err(why) => {
            return format!(
                "<div class='card mcp-server'>{header}<div class='mcp-server-rows'><div class='pill-row'>{}</div></div>\
<div class='mcp-server-actions'>{remove}</div></div>",
                pill("pill-red", &ctx.tf("Invalid entry: {why}", &[("why", why)]))
            )
        }
        Ok(s) => s,
    };
    let mut rows = vec![install_html(ctx, spec, env)];
    if spec.is_playwright() {
        rows.push(match spec.chrome_path().filter(|c| c.is_file()) {
            Some(c) => format!(
                "<div class='pill-row'><span class='mcp-label'>Chrome</span><code>{}</code></div>",
                h(&c.to_string_lossy())
            ),
            None => format!(
                "<div class='pill-row'>{}</div>",
                pill(
                    "pill-red",
                    &ctx.t("Google Chrome was not found: install it in /Applications or set LOOPX_CHROME")
                )
            ),
        });
    }
    if !spec.env_secrets.is_empty() {
        let chips: String = spec
            .env_secrets
            .iter()
            .map(|(var, account)| {
                if stored.contains(account) {
                    pill("pill-green", &ctx.tf("Secret set: {var}", &[("var", var)]))
                } else {
                    pill("pill-red", &ctx.tf("Secret missing: {var}", &[("var", var)]))
                }
            })
            .collect();
        rows.push(format!("<div class='pill-row'>{chips}</div>"));
    }
    rows.push(format!("<div class='pill-row'>{}</div>", result_html(ctx, result)));
    let used = match used.filter(|u| !u.is_empty()) {
        Some(list) => {
            let names: Vec<String> = list
                .iter()
                .map(|n| if n == "Fix now" { ctx.t("Fix now") } else { n.clone() })
                .collect();
            ctx.tf("Used by {loops}", &[("loops", &names.join(", "))])
        }
        None => ctx.t("Not used by any loop"),
    };
    rows.push(format!("<p class='mcp-hint'>{}</p>", h(&used)));
    let test = action_form(
        ctx,
        "/connectors/mcp/test",
        name,
        "btn-neutral",
        "check_circle",
        &ctx.t("Test"),
        None,
    );
    format!(
        "<div class='card mcp-server'>{header}<div class='mcp-server-rows'>{}</div>\
<div class='mcp-server-actions'>{test}{remove}</div></div>",
        rows.join("")
    )
}

fn add_card(ctx: &UiCtx) -> String {
    let chrome = chrome(ctx.state);
    let (disabled, hint) = match &chrome {
        Some(_) => (String::new(), String::new()),
        None => (
            " disabled".to_string(),
            format!(
                "<p class='mcp-hint'>{}</p>",
                h(&ctx.t("Google Chrome was not found: install it in /Applications or set LOOPX_CHROME"))
            ),
        ),
    };
    format!(
        "<div class='card mcp-server'><div class='section-header'>{}<h2>{}</h2></div>\
<div class='mcp-server-rows'><p class='mcp-hint'>{}</p>{}{hint}</div>\
<div class='mcp-server-actions'><form method='post' action='/connectors/mcp/add-playwright'>{}\
<button type='submit' class='btn btn-primary'{disabled}>{} {}</button></form></div></div>",
        icons::icon("add"),
        h(&ctx.t("Add Playwright")),
        h(&ctx.t("Browser automation on this Mac's Google Chrome, the fallback for before/after screenshots. No browser is downloaded.")),
        install_block(ctx, reg::PLAYWRIGHT_INSTALL_CMD),
        ctx.csrf_input(),
        icons::icon("add"),
        h(&ctx.t("Add Playwright"))
    )
}

pub fn body(ctx: &UiCtx, flash: Option<&str>, flash_ok: bool) -> String {
    let env = env(ctx.state);
    let (servers, error_html) = match reg::load(&env.registry) {
        Ok(r) => (r.servers, String::new()),
        Err(e) => (BTreeMap::new(), flash_html(Some(&e), false)),
    };
    let stored = env.secrets.list().unwrap_or_default();
    let results = mcp_probe::load_results(&results_path(ctx.state));
    let used = used_by(ctx.state);
    let mut cards: Vec<String> = servers
        .iter()
        .map(|(name, spec)| server_card(ctx, name, spec, &env, &stored, results.get(name), used.get(name)))
        .collect();
    if servers.is_empty() && error_html.is_empty() {
        cards.push(format!(
            "<div class='card'><p class='section-subtitle'>{}</p></div>",
            h(&ctx.t("No MCP servers yet. Servers are listed in ~/.loopx/mcp.json; loops opt in with mcp_servers in loop.yaml."))
        ));
    }
    if !servers.contains_key("playwright") {
        cards.push(add_card(ctx));
    }
    format!(
        "{}{error_html}<p class='section-subtitle'>{}</p><div class='grid connector-accounts'>{}</div>",
        flash_html(flash, flash_ok),
        h(&ctx.t("Tools a loop's Claude runs can use. Only the servers a loop declares attach, and only to that run.")),
        cards.join("")
    )
}

fn name_param(ctx: &UiCtx) -> String {
    ctx.form().first("name").unwrap_or("").to_string()
}

fn test(ctx: &UiCtx) -> UiResponse {
    let name = name_param(ctx);
    let r = mcp_probe::probe(&name, &env(ctx.state), ctx.state.mcp_probe_deadline);
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    if reg::valid_name(&name) {
        let _ = mcp_probe::save_result(&results_path(ctx.state), &name, &r, &at);
    }
    let message = if r.ok {
        ctx.tf("Test passed: {n} tools", &[("n", &r.tools.len().to_string())])
    } else {
        let first = r.message.lines().next().unwrap_or("").to_string();
        ctx.tf("Test failed: {message}", &[("message", &first)])
    };
    UiResponse::flash_redirect(r.ok, &message, BACK)
}

fn add_playwright(ctx: &UiCtx) -> UiResponse {
    let saved = reg::playwright_preset(chrome(ctx.state))
        .map_err(|_| ctx.t("Google Chrome was not found: install it in /Applications or set LOOPX_CHROME"))
        .and_then(|spec| reg::save_entry(&reg::path(&ctx.state.home_dir), "playwright", &spec));
    match saved {
        Ok(()) => UiResponse::flash_redirect(true, &ctx.tf("Added {name}", &[("name", "playwright")]), BACK),
        Err(e) => UiResponse::flash_redirect(false, &e, BACK),
    }
}

fn remove(ctx: &UiCtx) -> UiResponse {
    let name = name_param(ctx);
    match reg::remove_entry(&reg::path(&ctx.state.home_dir), &name) {
        Ok(true) => {
            let _ = mcp_probe::remove_result(&results_path(ctx.state), &name);
            UiResponse::flash_redirect(true, &ctx.tf("Removed {name}", &[("name", &name)]), BACK)
        }
        Ok(false) => UiResponse::flash_redirect(false, &ctx.tf("No MCP server named {name}", &[("name", &name)]), BACK),
        Err(e) => UiResponse::flash_redirect(false, &e, BACK),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::routes::UiRequest;
    use axum::http::Method;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn exec(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn fake_server() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/fake_mcp_server.sh")
    }

    /// A global install of `@playwright/mcp@version` under `<home>/.nvm`, whose
    /// `node` runs the fake MCP server.
    fn install(home: &Path, version: &str) {
        let prefix = home.join(".nvm/versions/node/v20.15.0");
        let pkg = prefix.join("lib/node_modules/@playwright/mcp");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            format!(r#"{{"name": "@playwright/mcp", "version": "{version}"}}"#),
        )
        .unwrap();
        std::fs::write(pkg.join("cli.js"), "").unwrap();
        exec(
            &prefix.join("bin/node"),
            &format!(
                "[ \"$1\" = -v ] && {{ echo v20.15.0; exit 0; }}\nexec sh '{}'",
                fake_server().display()
            ),
        );
        std::os::unix::fs::symlink(
            "../lib/node_modules/@playwright/mcp/cli.js",
            prefix.join("bin/playwright-mcp"),
        )
        .unwrap();
    }

    struct T {
        dir: tempfile::TempDir,
        state: UiState,
        store: Arc<loopx_core::keychain::MemoryStore>,
        chrome: PathBuf,
    }

    impl T {
        fn new() -> T {
            let dir = tempfile::tempdir().unwrap();
            let d = dir.path();
            let mut state = UiState::explicit(d, &d.join("lx"), &d.join("home"), &d.join("update"), "tok", None);
            let store = Arc::new(loopx_core::keychain::MemoryStore::new(reg::SERVICE_MCP));
            state.mcp_store = Some(store.clone());
            let chrome = d.join("Chrome");
            std::fs::write(&chrome, "").unwrap();
            state.mcp_chrome = Some(Some(chrome.clone()));
            T { dir, state, store, chrome }
        }

        fn registry(&self) -> PathBuf {
            reg::path(&self.state.home_dir)
        }

        fn add_playwright(&self) {
            reg::save_entry(&self.registry(), "playwright", &reg::playwright_preset(Some(self.chrome.clone())).unwrap()).unwrap();
        }

        fn add_demo(&self) {
            let exe = self.dir.path().join("demo-mcp");
            exec(&exe, &format!("exec sh '{}'", fake_server().display()));
            let spec = ServerSpec::from_value(&serde_json::json!({
                "command": exe, "env": {"FAKE_MODE": "ok"},
                "env_secrets": {"DEMO_TOKEN": "keychain:demo-token"}}))
            .unwrap();
            reg::save_entry(&self.registry(), "demo", &spec).unwrap();
        }

        fn page(&self) -> String {
            self.state.registry.dispatch(&self.state, &UiRequest::get(BACK)).unwrap().body_text()
        }

        fn post(&self, path: &str, body: &str) -> UiResponse {
            self.state
                .registry
                .dispatch(&self.state, &UiRequest::new(Method::POST, path).with_body(body.to_string()))
                .unwrap()
        }
    }

    fn location(r: &UiResponse) -> String {
        r.header("Location").unwrap().to_string()
    }

    #[test]
    fn mcp_view_lists_servers_with_install_and_secret_status() {
        let t = T::new();
        install(&t.state.user_home, reg::PLAYWRIGHT_VERSION);
        t.add_playwright();
        t.add_demo();
        let html = t.page();
        assert!(html.contains("<h2>playwright</h2>"), "{html}");
        assert!(html.contains("<h2>demo</h2>"), "{html}");
        assert!(html.contains("Installed 0.0.84"), "{html}");
        assert!(html.contains("v20.15.0"), "{html}");
        assert!(html.contains(&*t.chrome.to_string_lossy()), "{html}");
        assert!(html.contains("Secret missing: DEMO_TOKEN"), "{html}");
        assert!(html.contains("Never tested"), "{html}");
        assert!(html.contains("class='mcp-server-actions'"), "{html}");
        assert!(!html.contains("Add Playwright</button>"), "no add card once playwright is registered");
        t.store.set("demo-token", "s3cret-value").unwrap();
        let html = t.page();
        assert!(html.contains("Secret set: DEMO_TOKEN"), "{html}");
    }

    #[test]
    fn not_installed_shows_the_exact_install_command() {
        let t = T::new();
        t.add_playwright();
        let html = t.page();
        assert!(html.contains("Not installed"), "{html}");
        assert!(html.contains(&format!("<code>{}</code>", reg::PLAYWRIGHT_INSTALL_CMD)), "{html}");
        assert!(html.contains("Run this once in Terminal, then press Test."), "{html}");
    }

    #[test]
    fn wrong_version_shows_found_and_needed() {
        let t = T::new();
        install(&t.state.user_home, "0.0.83");
        t.add_playwright();
        let html = t.page();
        assert!(html.contains("Wrong version: found 0.0.83, needs 0.0.84"), "{html}");
        assert!(html.contains(reg::PLAYWRIGHT_INSTALL_CMD), "{html}");
    }

    #[test]
    fn the_page_never_shows_a_secret_value() {
        let t = T::new();
        t.add_demo();
        t.store.set("demo-token", "s3cret-value").unwrap();
        let r = t.post("/connectors/mcp/test", "csrf_token=tok&name=demo");
        assert!(!location(&r).contains("s3cret-value"));
        let html = t.page();
        assert!(!html.contains("s3cret-value"), "{html}");
        let saved = std::fs::read_to_string(mcp_probe::results_path(&t.state.repo_root)).unwrap();
        assert!(!saved.contains("s3cret-value"));
    }

    #[test]
    fn test_button_runs_the_probe_and_shows_tools() {
        let t = T::new();
        t.add_demo();
        t.store.set("demo-token", "s3cret-value").unwrap();
        let r = t.post("/connectors/mcp/test", "csrf_token=tok&name=demo");
        assert_eq!(r.status, 303);
        assert!(location(&r).starts_with("/connectors?view=mcp&flash=Test%20passed"), "{}", location(&r));
        let html = t.page();
        assert!(html.contains("Test passed: 3 tools"), "{html}");
        assert!(html.contains("browser_navigate"), "{html}");
        assert!(html.contains("token_ok"), "{html}");
        t.store.delete("demo-token").unwrap();
        let r = t.post("/connectors/mcp/test", "csrf_token=tok&name=demo");
        assert!(location(&r).contains("ok=0"), "{}", location(&r));
        assert!(t.page().contains("Test failed"), "the failed run replaces the last result");
    }

    #[test]
    fn mcp_posts_without_csrf_are_refused() {
        let t = T::new();
        t.add_demo();
        for path in ["/connectors/mcp/test", "/connectors/mcp/add-playwright", "/connectors/mcp/remove"] {
            let r = t.post(path, "name=demo");
            assert_eq!(r.status, 403, "{path}");
        }
        assert!(reg::load(&t.registry()).unwrap().servers.contains_key("demo"));
    }

    #[test]
    fn add_playwright_writes_the_pinned_preset_and_is_disabled_without_chrome() {
        let mut t = T::new();
        let html = t.page();
        assert!(html.contains("No MCP servers yet."), "{html}");
        assert!(html.contains(reg::PLAYWRIGHT_INSTALL_CMD), "{html}");
        assert!(html.contains("action='/connectors/mcp/add-playwright'"), "{html}");
        let r = t.post("/connectors/mcp/add-playwright", "csrf_token=tok");
        assert!(location(&r).contains("Added%20playwright"), "{}", location(&r));
        assert_eq!(
            reg::load(&t.registry()).unwrap().servers["playwright"],
            Ok(reg::playwright_preset(Some(t.chrome.clone())).unwrap())
        );
        std::fs::remove_file(t.registry()).unwrap();
        t.state.mcp_chrome = Some(None);
        let html = t.page();
        assert!(html.contains("disabled"), "{html}");
        assert!(html.contains("Google Chrome was not found: install it in /Applications or set LOOPX_CHROME"), "{html}");
        let r = t.post("/connectors/mcp/add-playwright", "csrf_token=tok");
        assert!(location(&r).contains("ok=0"));
        assert!(!t.registry().exists());
    }

    #[test]
    fn remove_keeps_other_entries() {
        let t = T::new();
        t.add_playwright();
        t.add_demo();
        let r = t.post("/connectors/mcp/remove", "csrf_token=tok&name=playwright");
        assert!(location(&r).contains("Removed%20playwright"), "{}", location(&r));
        let servers = reg::load(&t.registry()).unwrap().servers;
        assert!(!servers.contains_key("playwright"));
        assert!(servers.contains_key("demo"));
    }

    #[test]
    fn used_by_lists_fix_now_with_the_issue_loop() {
        let t = T::new();
        t.add_playwright();
        t.add_demo();
        let def = t.state.repo_root.join(loopx_core::issue_runner::DEFINITION_RELATIVE_PATH);
        std::fs::create_dir_all(def.parent().unwrap()).unwrap();
        std::fs::write(
            &def,
            "name: gitlab-issue-loop\nversion: 1\ntrigger: {type: manual}\ngoal: {type: issue_resolution}\nmcp_servers: [playwright]\n",
        )
        .unwrap();
        let html = t.page();
        let issue = loopx_core::loops_config::display_name("gitlab-issue-loop");
        assert!(html.contains(&format!("Used by {issue}, Fix now")), "{html}");
        assert!(html.contains("Not used by any loop"), "{html}");
    }

    #[test]
    fn invalid_entry_shows_why_and_only_remove() {
        let t = T::new();
        std::fs::create_dir_all(&t.state.home_dir).unwrap();
        std::fs::write(t.registry(), r#"{"bad": {"command": "npx"}}"#).unwrap();
        let html = t.page();
        assert!(html.contains("Invalid entry: command"), "{html}");
        assert!(html.contains("action='/connectors/mcp/remove'"), "{html}");
        assert!(!html.contains("action='/connectors/mcp/test'"), "{html}");
    }
}
