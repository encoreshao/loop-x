//! Settings → AI CLI → "Claude account & usage": who Claude Code is signed in as and
//! how much of the 5-hour and weekly subscription usage is used (what `/status` and
//! `/usage` show in the terminal). Shown only while Claude is the selected CLI.
//! Display only; "Subscription usage" below it is what pauses scheduled runs.
//!
//! Every Settings tab is in the page at once, so the card does not start its fetch
//! with the shell's page-load `data-lazy-load`: its own script fetches the fragment
//! when the AI CLI tab is first shown, and again on Refresh. The fetch runs
//! `claude auth status` and reads the Keychain token, so it never runs for a visit
//! that does not look at this tab.

use crate::ui::consts;
use crate::ui::pyfmt::escape as h;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use chrono::{DateTime, Local, Utc};
use loopx_core::account_switch;
use loopx_core::ai_cli_config;
use loopx_core::claude_status::{self, Account, AccountState, Snapshot};
use loopx_core::quota::Window;

pub const FRAGMENT_URL: &str = "/ai-cli/claude-status";

/// A window this full or fuller gets the warning colour.
const HIGH_PERCENT: f64 = 80.0;

pub fn register(r: &mut Registry) {
    r.get(FRAGMENT_URL, fragment);
}

/// The card, empty but for a spinner; its script loads the fragment.
pub fn card(ctx: &UiCtx) -> String {
    let accounts = account_switch::accounts(&ai_cli_config::load_config(&ctx.state.ai_cli_config_path()));
    let switcher = if accounts.len() < 2 {
        String::new()
    } else {
        let buttons: String = accounts
            .iter()
            .enumerate()
            .map(|(i, a)| {
                // The default account is the empty name, so the request carries no `account`.
                let (value, label) = if i == 0 { (String::new(), ctx.t("Default")) } else { (a.name.clone(), a.name.clone()) };
                format!(
                    "<button type='button' class='pref-segmented-option{}' data-claude-pick='{}' aria-pressed='{}'>{}</button>",
                    if i == 0 { " is-active" } else { "" },
                    h(&value),
                    i == 0,
                    h(&label)
                )
            })
            .collect();
        format!(
            "<div class='pref-segmented claude-switcher' role='group' aria-label='{}'>{buttons}</div>",
            h(&ctx.t("Claude account"))
        )
    };
    let load_error = serde_json::to_string(&format!(
        "<p class='inline-error'>{}</p>",
        h(&ctx.t("Couldn't load this section."))
    ))
    .unwrap_or_default()
    .replace('<', "\\u003c");
    format!(
        "<section class='card' data-claude-account>\
<div class='section-header'>{icon}<h2>{title}</h2></div>\
<p class='section-subtitle'>{subtitle}</p>{switcher}\
<div data-claude-body data-lazy-src='{FRAGMENT_URL}'><div class='lazy-loading'><div class='md-spinner'></div>\
<p class='loading-text'>{loading}<span class='loading-dots'><span>.</span><span>.</span><span>.</span></span></p></div></div>\
<div class='claude-actions'><button type='button' class='btn btn-neutral' data-claude-refresh>{refresh}</button></div>\
<script>
(function() {{
  var card = document.querySelector('[data-claude-account]');
  if (!card) return;
  var body = card.querySelector('[data-claude-body]');
  var button = card.querySelector('[data-claude-refresh]');
  var spinner = body.innerHTML;
  var busy = false;
  var account = '';
  function load() {{
    if (busy) return;
    busy = true;
    button.disabled = true;
    body.innerHTML = spinner;
    var url = body.getAttribute('data-lazy-src') + (account ? '?account=' + encodeURIComponent(account) : '');
    fetch(url)
      .then(function(r) {{ return r.text(); }})
      .then(function(html) {{ body.innerHTML = html; }})
      .catch(function() {{ body.innerHTML = {load_error}; }})
      .then(function() {{ busy = false; button.disabled = false; }});
  }}
  button.addEventListener('click', load);
  card.querySelectorAll('[data-claude-pick]').forEach(function(pick) {{
    pick.addEventListener('click', function() {{
      if (busy) return;
      account = pick.getAttribute('data-claude-pick');
      card.querySelectorAll('[data-claude-pick]').forEach(function(other) {{
        var on = other === pick;
        other.classList.toggle('is-active', on);
        other.setAttribute('aria-pressed', on ? 'true' : 'false');
      }});
      load();
    }});
  }});
  // The AI CLI tab is one of several panels in the page: fetch only once it is shown.
  var panel = card.closest('[data-tab-panel]');
  if (!panel || !panel.hidden) {{ load(); return; }}
  document.addEventListener('click', function opened(ev) {{
    if (!ev.target.closest('[data-tab-target=\"ai-cli\"]')) return;
    document.removeEventListener('click', opened);
    load();
  }});
}})();
</script></section>",
        icon = consts::ds("_SECTION_ICON_AI_CLI"),
        switcher = switcher,
        title = h(&ctx.t("Claude account & usage")),
        subtitle = h(&ctx.t("The account Claude Code is signed in as and how much of your subscription is used, as /status and /usage show it. The Subscription usage setting is what pauses scheduled runs.")),
        loading = h(&ctx.t("Loading your Claude account and usage")),
        refresh = h(&ctx.t("Refresh")),
    )
}

/// "Claude Team account" for a subscription sign-in, else the method as reported.
fn login_method(ctx: &UiCtx, a: &Account) -> String {
    if a.method != "claude.ai" {
        return a.method.clone();
    }
    match a.plan.as_deref() {
        Some(plan) => {
            let mut chars = plan.chars();
            let plan = chars.next().map(|c| c.to_uppercase().chain(chars).collect::<String>()).unwrap_or_default();
            ctx.tf("Claude {plan} account", &[("plan", &plan)])
        }
        None => ctx.t("Claude account"),
    }
}

fn fact(label: String, value: &str) -> String {
    format!("<dt>{}</dt><dd>{}</dd>", h(&label), h(value))
}

fn account_html(ctx: &UiCtx, a: &Account) -> String {
    let mut rows = String::new();
    if let Some(email) = &a.email {
        rows.push_str(&fact(ctx.t("Email"), email));
    }
    if let Some(org) = &a.org {
        rows.push_str(&fact(ctx.t("Organization"), org));
    }
    rows.push_str(&fact(ctx.t("Login method"), &login_method(ctx, a)));
    format!("<dl class='claude-facts'>{rows}</dl>")
}

/// "9:39pm" when `at` is today, else "Nov 9 at 1:59pm", in local time.
fn reset_text(now: DateTime<Utc>, at: DateTime<Utc>) -> String {
    let (now, at) = (now.with_timezone(&Local), at.with_timezone(&Local));
    if now.date_naive() == at.date_naive() {
        at.format("%-I:%M%P").to_string()
    } else {
        at.format("%b %-d at %-I:%M%P").to_string()
    }
}

fn meter_html(ctx: &UiCtx, w: &Window) -> String {
    let name = match w.label {
        "5-hour" => ctx.t("Current session"),
        "weekly" => ctx.t("Current week"),
        other => other.to_string(),
    };
    let pct = w.used_percent.round() as i64;
    let resets = w
        .resets_at
        .map(|at| {
            format!(
                "<p class='usage-meter-reset'>{}</p>",
                h(&ctx.tf("Resets {when}", &[("when", &reset_text(ctx.state.now(), at))]))
            )
        })
        .unwrap_or_default();
    format!(
        "<div class='usage-meter{high}'><div class='usage-meter-head'><span>{name}</span><span>{used}</span></div>\
<div class='usage-meter-track' role='progressbar' aria-label='{name}' aria-valuemin='0' aria-valuemax='100' aria-valuenow='{pct}'>\
<div class='usage-meter-fill' style='width:{pct}%'></div></div>{resets}</div>",
        high = if w.used_percent >= HIGH_PERCENT { " is-high" } else { "" },
        name = h(&name),
        used = h(&ctx.tf("{n}% used", &[("n", &pct.to_string())])),
    )
}

fn notice(text: &str) -> String {
    format!("<p class='section-subtitle'>{}</p>", h(text))
}

/// The account and usage in `snap`.
pub fn fragment_html(ctx: &UiCtx, snap: &Snapshot) -> String {
    let account = match &snap.account {
        Err(e) => {
            return notice(&format!("{}: {e}", ctx.t("Could not read the Claude account")));
        }
        Ok(AccountState::SignedOut) => {
            return format!(
                "<p class='section-subtitle'>{}</p>",
                ctx.tf(
                    "Not signed in. Open Terminal and run {cmd}; scheduled runs fail until then.",
                    &[("cmd", "<code>claude auth login</code>")]
                )
            );
        }
        Ok(AccountState::SignedIn(a)) => account_html(ctx, a),
    };
    let usage = match &snap.usage {
        Ok(windows) => format!(
            "<div class='usage-meters'>{}</div>",
            windows.iter().map(|w| meter_html(ctx, w)).collect::<String>()
        ),
        Err(e) => notice(&ctx.tf("Usage unavailable: {reason}", &[("reason", e)])),
    };
    format!("{account}{usage}")
}

fn live(account: &account_switch::Account) -> Snapshot {
    let path = loopx_core::ai_cli_health::login_shell_path(
        "zsh",
        &std::env::var("PATH").unwrap_or_default(),
    );
    claude_status::snapshot_in(&path, account.config_dir.as_deref(), &|| {
        account_switch::live_usage(account)
    })
}

/// The registered account named by `?account=`; the default one when absent or unknown.
fn requested_account(ctx: &UiCtx) -> account_switch::Account {
    let accounts = account_switch::accounts(&ai_cli_config::load_config(&ctx.state.ai_cli_config_path()));
    let wanted = ctx.query().first("account").unwrap_or("").to_string();
    let default = accounts[0].clone();
    accounts.into_iter().skip(1).find(|a| a.name == wanted).unwrap_or(default)
}

fn fragment(ctx: &UiCtx) -> UiResponse {
    let snap = ctx.state.claude_snapshot.clone().unwrap_or_else(|| live(&requested_account(ctx)));
    UiResponse::html(fragment_html(ctx, &snap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{UiRequest, UiState};
    use chrono::{TimeZone, Utc};
    use loopx_core::claude_status::{Account, AccountState};
    use loopx_core::quota::Window;

    fn state(dir: &std::path::Path) -> UiState {
        UiState::explicit(dir, dir, dir, dir, "tok", Some(Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap()))
    }

    fn team() -> Account {
        Account {
            method: "claude.ai".into(),
            email: Some("loopx@ekohe.com".into()),
            org: Some("Ekohe".into()),
            plan: Some("team".into()),
        }
    }

    fn window(label: &'static str, used: f64, resets: Option<(u32, u32, u32)>) -> Window {
        Window {
            label,
            used_percent: used,
            resets_at: resets.map(|(d, h, m)| Utc.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap()),
        }
    }

    fn signed_in(usage: Result<Vec<Window>, String>) -> Snapshot {
        Snapshot { account: Ok(AccountState::SignedIn(team())), usage }
    }

    fn render(snap: &Snapshot, cookie: Option<&str>) -> String {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let mut req = UiRequest::get("/ai-cli/claude-status");
        if let Some(c) = cookie {
            req = req.with_header("cookie", c);
        }
        fragment_html(&UiCtx::new(&st, &req), snap)
    }

    #[test]
    fn the_card_is_a_spinner_that_loads_its_fragment_when_the_tab_opens() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let req = UiRequest::get("/settings?tab=ai-cli");
        let html = card(&UiCtx::new(&st, &req));
        assert!(html.contains("data-claude-account"), "{html}");
        assert!(html.contains("Claude account &amp; usage"), "{html}");
        assert!(html.contains("data-lazy-src='/ai-cli/claude-status'"), "{html}");
        assert!(
            !html.contains("data-lazy-load"),
            "the shell's page-load handler would fetch it on every Settings visit: {html}"
        );
        assert!(html.contains("md-spinner") && html.contains("Loading your Claude account and usage"), "{html}");
        assert!(html.contains("data-claude-refresh") && html.contains(">Refresh<"), "{html}");
    }

    fn state_with_accounts(dir: &std::path::Path) -> UiState {
        let st = state(dir);
        std::fs::write(
            st.ai_cli_config_path(),
            r#"{"claude_accounts": [{"name": "Work", "config_dir": "/w"}, {"name": "A&B", "config_dir": "/ab"}]}"#,
        )
        .unwrap();
        st
    }

    #[test]
    fn one_account_has_no_switcher() {
        let d = tempfile::tempdir().unwrap();
        let st = state(d.path());
        let html = card(&UiCtx::new(&st, &UiRequest::get("/settings?tab=ai-cli")));
        assert!(!html.contains("pref-segmented-option"), "{html}");
    }

    #[test]
    fn several_accounts_get_a_switcher_with_the_default_selected() {
        let d = tempfile::tempdir().unwrap();
        let st = state_with_accounts(d.path());
        let html = card(&UiCtx::new(&st, &UiRequest::get("/settings?tab=ai-cli")));
        assert_eq!(html.matches("pref-segmented-option").count(), 3, "{html}");
        assert!(html.contains("data-claude-pick='' aria-pressed='true'"), "{html}");
        assert!(html.contains("data-claude-pick='Work' aria-pressed='false'"), "{html}");
        assert!(html.contains("data-claude-pick='A&amp;B'"), "names are escaped: {html}");
    }

    #[test]
    fn the_fragment_for_an_unknown_account_falls_back_to_the_default() {
        let d = tempfile::tempdir().unwrap();
        let mut st = state_with_accounts(d.path());
        st.claude_snapshot = Some(signed_in(Ok(vec![window("5-hour", 7.0, None)])));
        for url in [format!("{FRAGMENT_URL}?account=Work"), format!("{FRAGMENT_URL}?account=Nobody")] {
            let res = st.registry.dispatch(&st, &UiRequest::get(&url)).unwrap();
            assert_eq!(res.status, 200, "{url}");
            assert!(res.body_text().contains("7% used"), "{url}");
        }
    }

    #[test]
    fn a_signed_in_account_shows_who_and_the_plan() {
        let html = render(&signed_in(Ok(vec![])), None);
        assert!(html.contains("loopx@ekohe.com"), "{html}");
        assert!(html.contains("Ekohe"), "{html}");
        assert!(html.contains("Claude Team account"), "{html}");
    }

    #[test]
    fn a_sign_in_without_a_plan_is_a_plain_claude_account_and_other_methods_show_as_is() {
        let mut a = team();
        a.plan = None;
        let snap = Snapshot { account: Ok(AccountState::SignedIn(a.clone())), usage: Ok(vec![]) };
        assert!(render(&snap, None).contains("Claude account"), "no plan");
        a.method = "apiKey".into();
        let snap = Snapshot { account: Ok(AccountState::SignedIn(a)), usage: Ok(vec![]) };
        assert!(render(&snap, None).contains("apiKey"));
    }

    #[test]
    fn usage_is_a_bar_per_window_with_its_percent() {
        let html = render(
            &signed_in(Ok(vec![window("5-hour", 42.0, None), window("weekly", 19.4, None)])),
            None,
        );
        assert!(html.contains("Current session") && html.contains("42% used"), "{html}");
        assert!(html.contains("Current week") && html.contains("19% used"), "{html}");
        assert!(html.contains("style='width:42%'") && html.contains("style='width:19%'"), "{html}");
        assert_eq!(html.matches("role='progressbar'").count(), 2, "{html}");
        assert!(html.contains("aria-valuenow='42'"), "{html}");
    }

    #[test]
    fn a_window_near_its_limit_is_marked_high() {
        let html = render(
            &signed_in(Ok(vec![window("5-hour", 100.0, None), window("weekly", 19.0, None)])),
            None,
        );
        assert_eq!(html.matches("is-high").count(), 1, "{html}");
        let high = &html[html.find("is-high").unwrap()..];
        assert!(high.find("100% used").unwrap() < high.find("19% used").unwrap(), "{html}");
    }

    #[test]
    fn a_reset_today_shows_the_time_and_a_later_one_the_date_too() {
        // 2026-11-02 12:00 UTC is "now"; one reset later today, one a week out.
        let soon = Utc.with_ymd_and_hms(2026, 11, 2, 13, 39, 0).unwrap();
        let later = Utc.with_ymd_and_hms(2026, 11, 9, 5, 59, 0).unwrap();
        let local = |t: chrono::DateTime<Utc>, f: &str| t.with_timezone(&chrono::Local).format(f).to_string();
        let html = render(
            &signed_in(Ok(vec![window("5-hour", 10.0, Some((2, 13, 39))), window("weekly", 10.0, Some((9, 5, 59)))])),
            None,
        );
        let now = Utc.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap();
        // Whether 13:39 UTC is still "today" depends on the machine's time zone.
        let expected_soon = if local(now, "%F") == local(soon, "%F") {
            local(soon, "%-I:%M%P")
        } else {
            local(soon, "%b %-d at %-I:%M%P")
        };
        assert!(html.contains(&format!("Resets {expected_soon}<")), "{html}");
        let dated = local(later, "%b %-d at %-I:%M%P");
        assert!(html.contains(&format!("Resets {dated}<")), "{html}");
    }

    #[test]
    fn a_usage_failure_keeps_the_account_and_says_why() {
        let html = render(&signed_in(Err("no Claude Code sign-in found".into())), None);
        assert!(html.contains("loopx@ekohe.com"), "{html}");
        assert!(html.contains("Usage unavailable: no Claude Code sign-in found"), "{html}");
        assert!(!html.contains("role='progressbar'"), "{html}");
    }

    #[test]
    fn signed_out_points_at_the_login_command() {
        let snap = Snapshot {
            account: Ok(AccountState::SignedOut),
            usage: Err("not signed in".into()),
        };
        let html = render(&snap, None);
        assert!(html.contains("Not signed in") && html.contains("claude auth login"), "{html}");
        assert!(!html.contains("Usage unavailable"), "the sign-in hint is the whole story: {html}");
    }

    #[test]
    fn an_account_that_could_not_be_read_shows_the_reason() {
        let snap = Snapshot {
            account: Err("claude is not installed or not on your login shell's PATH".into()),
            usage: Err("x".into()),
        };
        let html = render(&snap, None);
        assert!(html.contains("Could not read the Claude account"), "{html}");
        assert!(html.contains("claude is not installed"), "{html}");
    }

    #[test]
    fn account_text_is_html_escaped() {
        let mut a = team();
        a.email = Some("<b>x</b>@e.com".into());
        a.org = Some("A & B".into());
        let snap = Snapshot { account: Ok(AccountState::SignedIn(a)), usage: Ok(vec![]) };
        let html = render(&snap, None);
        assert!(html.contains("&lt;b&gt;x&lt;/b&gt;@e.com") && html.contains("A &amp; B"), "{html}");
        assert!(!html.contains("<b>x"), "{html}");
    }

    #[test]
    fn the_route_serves_the_pinned_snapshot_as_a_fragment() {
        let d = tempfile::tempdir().unwrap();
        let mut st = state(d.path());
        st.claude_snapshot = Some(signed_in(Ok(vec![window("5-hour", 7.0, None)])));
        let res = st.registry.dispatch(&st, &UiRequest::get(FRAGMENT_URL)).unwrap();
        assert_eq!(res.status, 200);
        let body = res.body_text();
        assert!(!body.contains("<html"), "a fragment, not a page");
        assert!(body.contains("loopx@ekohe.com") && body.contains("7% used"), "{body}");
    }
}
