//! Settings › General › Voice: which speech-to-text engine the chat mic uses, the local
//! Whisper model, the spoken language and the OpenAI key. The machine check (which Python,
//! which models) is a lazy fragment, so the page itself never starts Python.

use crate::ui::pages::loops_common::{custom_select, mask_secret};
use crate::ui::pyfmt::escape;
use crate::ui::routes::{Registry, UiCtx, UiResponse};
use crate::ui::voice::{secret_store, status_text};
use loopx_core::voice::{self, Engine, Setup, VoiceConfig, OPENAI_KEY_ACCOUNT};

const BACK: &str = "/settings?tab=voice";

const MODELS: [&str; 9] = [
    "tiny", "base", "small", "medium", "large-v3", "tiny.en", "base.en", "small.en", "medium.en",
];

const LANGUAGES: [(&str, &str); 12] = [
    ("en", "English"),
    ("zh", "中文"),
    ("ja", "日本語"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("es", "Español"),
    ("ko", "한국어"),
    ("pt", "Português"),
    ("it", "Italiano"),
    ("ru", "Русский"),
    ("nl", "Nederlands"),
    ("ar", "العربية"),
];

pub fn register(r: &mut Registry) {
    r.get("/settings/voice/status", status);
    r.post("/settings/voice", save);
    r.post("/settings/voice/forget-key", forget_key);
}

fn config_path(ctx: &UiCtx) -> std::path::PathBuf {
    ctx.state.home_dir.join("voice.json")
}

fn engine_label(ctx: &UiCtx, e: Engine) -> String {
    ctx.t(match e {
        Engine::Auto => "Automatic: local Whisper, then OpenAI",
        Engine::Local => "Local Whisper only",
        Engine::Openai => "OpenAI only",
        Engine::Off => "Off",
    })
}

fn field(label: &str, control: &str, hint: &str) -> String {
    format!(
        "<div class='stack-field'><span class='field-label'>{}</span>{control}<span class='field-hint'>{}</span></div>",
        escape(label),
        escape(hint)
    )
}

pub fn panel(ctx: &UiCtx, csrf: &str) -> String {
    let cfg = voice::load(&config_path(ctx));
    let engines: Vec<(String, String)> =
        Engine::ALL.iter().map(|e| (e.key().to_string(), engine_label(ctx, *e))).collect();
    let models: Vec<(String, String)> = MODELS.iter().map(|m| (m.to_string(), m.to_string())).collect();
    let languages: Vec<(String, String)> =
        LANGUAGES.iter().map(|(c, n)| (c.to_string(), n.to_string())).collect();
    let python = format!(
        "<input type='text' name='python' value='{}' placeholder='{}' autocomplete='off' spellcheck='false'>",
        escape(&cfg.python),
        escape(&ctx.t("Find automatically"))
    );
    let key = format!(
        "<input type='password' name='openai_api_key' value='' placeholder='{}' autocomplete='off'>",
        escape(&ctx.t("Leave blank to keep the saved key"))
    );
    format!(
        "<h3>{title}</h3><p class='section-subtitle'>{subtitle}</p>\
<div class='voice-status' data-lazy-load='/settings/voice/status'>{loading}</div>\
<form method='post' action='/settings/voice' class='stack-form'>{csrf}{engine}{model}{language}{python}{key}\
<div><button type='submit' class='btn btn-neutral'>{save}</button></div></form>",
        title = escape(&ctx.t("Voice input")),
        subtitle = escape(&ctx.t("Click the microphone in any chat box and speak. The recording is turned into text on this Mac with Whisper when it is installed, otherwise by OpenAI, and lands in the box for you to check before sending.")),
        loading = crate::ui::live_cache::placeholder(ctx, None, &ctx.t("Checking this Mac")),
        engine = field(
            &ctx.t("Speech-to-text"),
            &custom_select("engine", &engines, Some(cfg.engine.key()), None),
            &ctx.t("Automatic uses local Whisper when it is ready and falls back to OpenAI."),
        ),
        model = field(
            &ctx.t("Whisper model"),
            &custom_select("model", &models, Some(&cfg.model), None),
            &ctx.t("small is fast and handles every language. Models ending in .en are English only. A model that isn't downloaded falls back to one that is."),
        ),
        language = field(
            &ctx.t("Spoken language"),
            &custom_select("language", &languages, Some(&cfg.language), Some(&ctx.t("Detect automatically"))),
            &ctx.t("Picking your language helps with short recordings."),
        ),
        python = field(
            &ctx.t("Python with faster-whisper"),
            &python,
            &ctx.t("An absolute path, or leave blank to search PATH, pyenv and Homebrew."),
        ),
        key = field(
            &ctx.t("OpenAI API key"),
            &key,
            &ctx.t("Stored in the Keychain. Used when local Whisper isn't available."),
        ),
        save = escape(&ctx.t("Save")),
    )
}

fn status(ctx: &UiCtx) -> UiResponse {
    let cfg = voice::load(&config_path(ctx));
    let store = secret_store(ctx.state);
    let key = voice::openai_key(store.as_ref());
    let setup = Setup::detect_cached(&cfg, key.clone());
    let (ok, text) = status_text(ctx.lang, &cfg, &setup);
    let models = if setup.models.is_empty() {
        String::new()
    } else {
        format!(
            "<p class='field-hint'>{}</p>",
            escape(&ctx.tf("Downloaded models: {models}", &[("models", &setup.models.join(", "))]))
        )
    };
    let stored = store.get(OPENAI_KEY_ACCOUNT).ok().flatten().filter(|k| !k.trim().is_empty());
    let key_line = match stored {
        Some(k) => format!(
            "<form method='post' action='/settings/voice/forget-key' class='voice-key-form'>{}<span>{}</span>\
<button type='submit' class='btn btn-neutral btn-sm'>{}</button></form>",
            ctx.csrf_input(),
            escape(&ctx.tf("OpenAI key saved ({masked}).", &[("masked", &mask_secret(&k))])),
            escape(&ctx.t("Remove key")),
        ),
        None => String::new(),
    };
    UiResponse::html(format!(
        "<div class='flash {}' role='status'>{}</div>{models}{key_line}",
        if ok { "flash-success" } else { "flash-warning" },
        escape(&text)
    ))
}

fn save(ctx: &UiCtx) -> UiResponse {
    let form = ctx.form();
    let text = |k: &str| form.first(k).unwrap_or("").trim().to_string();
    let Some(engine) = Engine::parse(&text("engine")) else {
        return UiResponse::flash_redirect(false, &ctx.t("Pick a speech-to-text engine."), BACK);
    };
    let cfg = VoiceConfig {
        engine,
        model: text("model"),
        language: text("language"),
        python: text("python"),
    };
    if let Err(e) = voice::save(&cfg, &config_path(ctx)) {
        return UiResponse::flash_redirect(false, &ctx.tf("Couldn't save: {error}", &[("error", &e)]), BACK);
    }
    let key = text("openai_api_key");
    if !key.is_empty() {
        if let Err(e) = secret_store(ctx.state).set(OPENAI_KEY_ACCOUNT, &key) {
            return UiResponse::flash_redirect(false, &ctx.tf("Couldn't save: {error}", &[("error", &e.to_string())]), BACK);
        }
    }
    Setup::forget_detected();
    UiResponse::flash_redirect(true, &ctx.t("Voice input settings saved."), BACK)
}

fn forget_key(ctx: &UiCtx) -> UiResponse {
    let _ = secret_store(ctx.state).delete(OPENAI_KEY_ACCOUNT);
    UiResponse::flash_redirect(true, &ctx.t("OpenAI key removed."), BACK)
}

#[cfg(test)]
mod tests {
    use crate::ui::routes::UiRequest;
    use crate::ui::routes::UiResponse;
    use crate::ui::state::UiState;
    use axum::http::Method;
    use loopx_core::voice::{self, Engine, OPENAI_KEY_ACCOUNT};

    fn state(d: &std::path::Path) -> UiState {
        UiState::explicit(&d.join("repo"), &d.join("home"), &d.join("user"), &d.join("upd"), "tok", None)
    }

    fn location(r: &UiResponse) -> String {
        r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone()).unwrap_or_default()
    }

    fn post(s: &UiState, path: &str, body: &str) -> String {
        let req = UiRequest::new(Method::POST, path)
            .with_header("content-type", "application/x-www-form-urlencoded")
            .with_body(format!("csrf_token=tok&{body}"));
        location(&s.registry.dispatch(s, &req).expect("route"))
    }

    #[test]
    fn saving_writes_the_file_and_keeps_the_key_in_the_store() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        std::fs::create_dir_all(&s.home_dir).unwrap();
        let loc = post(&s, "/settings/voice", "engine=openai&model=small&language=ja&python=&openai_api_key=sk-1234");
        assert!(loc.starts_with("/settings?tab=voice") && loc.contains("ok=1"), "{loc}");
        let cfg = voice::load(&s.home_dir.join("voice.json"));
        assert_eq!((cfg.engine, cfg.language.as_str()), (Engine::Openai, "ja"));
        let store = s.voice_store.clone().unwrap();
        assert_eq!(store.get(OPENAI_KEY_ACCOUNT).unwrap().as_deref(), Some("sk-1234"));
        assert!(!std::fs::read_to_string(s.home_dir.join("voice.json")).unwrap().contains("sk-1234"));

        post(&s, "/settings/voice", "engine=auto&model=small&language=&python=&openai_api_key=");
        assert_eq!(store.get(OPENAI_KEY_ACCOUNT).unwrap().as_deref(), Some("sk-1234"), "blank keeps it");
        post(&s, "/settings/voice/forget-key", "");
        assert_eq!(store.get(OPENAI_KEY_ACCOUNT).unwrap(), None);
    }

    #[test]
    fn a_relative_python_path_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        let loc = post(&s, "/settings/voice", "engine=auto&model=small&language=&python=python3");
        assert!(loc.contains("ok=0"), "{loc}");
        assert!(!s.home_dir.join("voice.json").exists());
    }

    #[test]
    fn the_panel_loads_the_machine_check_lazily() {
        let d = tempfile::tempdir().unwrap();
        let s = state(d.path());
        let req = UiRequest::get("/settings?tab=voice");
        let html = String::from_utf8(s.registry.dispatch(&s, &req).unwrap().body).unwrap();
        assert!(html.contains("data-lazy-load='/settings/voice/status'"), "{html}");
        assert!(html.contains("name='openai_api_key' value=''"));
    }
}
