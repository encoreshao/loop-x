//! Speech-to-text for the chat composers (`<LOOP_ENGINEERING_HOME>/voice.json`): the user's
//! own faster-whisper install when it is there, otherwise OpenAI's transcription API.

pub mod local;
pub mod openai;

use crate::config::home_dir;
use crate::keychain::SecretStore;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_MODEL: &str = "small";
pub const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;
/// Account in the `loop-engineering.config` secret store.
pub const OPENAI_KEY_ACCOUNT: &str = "voice.openai_api_key";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Auto,
    Local,
    Openai,
    Off,
}

impl Engine {
    pub const ALL: [Engine; 4] = [Engine::Auto, Engine::Local, Engine::Openai, Engine::Off];

    pub fn key(self) -> &'static str {
        match self {
            Engine::Auto => "auto",
            Engine::Local => "local",
            Engine::Openai => "openai",
            Engine::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<Engine> {
        Engine::ALL.into_iter().find(|e| e.key() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceConfig {
    pub engine: Engine,
    pub model: String,
    /// ISO 639-1 code, or empty to let Whisper detect it.
    pub language: String,
    /// Interpreter with faster-whisper; empty to look for one.
    pub python: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            engine: Engine::Auto,
            model: DEFAULT_MODEL.into(),
            language: String::new(),
            python: String::new(),
        }
    }
}

pub fn default_config_path() -> PathBuf {
    home_dir().join("voice.json")
}

fn read_object(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

fn valid_model(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c))
}

fn valid_language(s: &str) -> bool {
    s.is_empty() || ((2..=3).contains(&s.len()) && s.chars().all(|c| c.is_ascii_lowercase()))
}

fn valid_python(s: &str) -> bool {
    s.is_empty() || Path::new(s).is_absolute()
}

pub fn load(path: &Path) -> VoiceConfig {
    let m = read_object(path);
    let text = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let d = VoiceConfig::default();
    VoiceConfig {
        engine: Engine::parse(&text("engine")).unwrap_or(d.engine),
        model: Some(text("model"))
            .filter(|s| valid_model(s))
            .unwrap_or(d.model),
        language: Some(text("language"))
            .filter(|s| valid_language(s))
            .unwrap_or(d.language),
        python: Some(text("python"))
            .filter(|s| valid_python(s))
            .unwrap_or(d.python),
    }
}

pub fn current() -> VoiceConfig {
    load(&default_config_path())
}

pub fn save(cfg: &VoiceConfig, path: &Path) -> Result<(), String> {
    if !valid_model(&cfg.model) {
        return Err(format!("not a model name: {}", cfg.model));
    }
    if !valid_language(&cfg.language) {
        return Err(format!("not a language code: {}", cfg.language));
    }
    if !valid_python(&cfg.python) {
        return Err(format!("the Python path must be absolute: {}", cfg.python));
    }
    let mut m = read_object(path);
    m.insert("engine".into(), json!(cfg.engine.key()));
    m.insert("model".into(), json!(cfg.model));
    m.insert("language".into(), json!(cfg.language));
    m.insert("python".into(), json!(cfg.python));
    crate::file_lock::write_atomic(
        path,
        crate::pyjson::to_string_pretty(&Value::Object(m)).as_bytes(),
    )
    .map_err(|e| e.to_string())
}

/// The stored OpenAI key, else `OPENAI_API_KEY`.
pub fn openai_key(store: &dyn SecretStore) -> Option<String> {
    store
        .get(OPENAI_KEY_ACCOUNT)
        .ok()
        .flatten()
        .or_else(|| std::env::var("OPENAI_API_KEY").ok())
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// Recording formats the composers send: (extension, MIME type for the upload).
pub fn audio_format(content_type: &str) -> Option<(&'static str, &'static str)> {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match base.as_str() {
        "audio/webm" | "video/webm" => ("webm", "audio/webm"),
        "audio/ogg" => ("ogg", "audio/ogg"),
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" | "audio/aac" | "video/mp4" => {
            ("m4a", "audio/mp4")
        }
        "audio/wav" | "audio/x-wav" | "audio/wave" => ("wav", "audio/wav"),
        "audio/mpeg" | "audio/mp3" => ("mp3", "audio/mpeg"),
        _ => return None,
    })
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

fn user_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// `name` on PATH, then in the Homebrew prefixes (launchd agents get a bare PATH).
fn find_program(name: &str) -> Option<PathBuf> {
    path_dirs()
        .into_iter()
        .chain([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ])
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn python_candidates(home: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = path_dirs()
        .into_iter()
        .filter(|d| !d.ends_with(".pyenv/shims"))
        .map(|d| d.join("python3"))
        .collect();
    let mut pyenv: Vec<PathBuf> = std::fs::read_dir(home.join(".pyenv/versions"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("bin/python3"))
        .collect();
    pyenv.sort();
    pyenv.reverse();
    out.extend(pyenv);
    out.extend(["/opt/homebrew/bin/python3", "/usr/local/bin/python3"].map(PathBuf::from));
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| p.is_file() && seen.insert(p.clone()));
    out
}

fn hf_hub(home: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("HF_HUB_CACHE") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("HF_HOME") {
        return PathBuf::from(p).join("hub");
    }
    home.join(".cache/huggingface/hub")
}

const DETECT_TTL: Duration = Duration::from_secs(300);
static DETECTED: Mutex<Option<(String, Instant, Setup)>> = Mutex::new(None);

/// What this machine can transcribe with, given the settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Setup {
    pub python: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
    pub models: Vec<String>,
    pub openai_key: Option<String>,
    pub openai_base: String,
}

impl Setup {
    /// Looks on disk (and starts each candidate Python once), so call it off the request path's
    /// hot loop: the first look can take a few seconds.
    pub fn detect(cfg: &VoiceConfig, openai_key: Option<String>) -> Setup {
        let home = user_home();
        let python = if cfg.python.is_empty() {
            python_candidates(&home)
                .into_iter()
                .find(|p| local::has_faster_whisper(p))
        } else {
            Some(PathBuf::from(&cfg.python)).filter(|p| local::has_faster_whisper(p))
        };
        Setup {
            python,
            ffmpeg: find_program("ffmpeg"),
            models: local::cached_models(&hf_hub(&home)),
            openai_key,
            openai_base: openai::DEFAULT_BASE.into(),
        }
    }

    /// [`Setup::detect`], reusing the machine part for a few minutes; the key is always fresh.
    pub fn detect_cached(cfg: &VoiceConfig, openai_key: Option<String>) -> Setup {
        let mut guard = DETECTED.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = guard
            .as_ref()
            .filter(|(python, at, _)| *python == cfg.python && at.elapsed() < DETECT_TTL)
            .map(|(_, _, s)| s.clone());
        let mut setup = fresh.unwrap_or_else(|| {
            let s = Setup::detect(cfg, None);
            *guard = Some((cfg.python.clone(), Instant::now(), s.clone()));
            s
        });
        setup.openai_key = openai_key;
        setup
    }

    pub fn forget_detected() {
        *DETECTED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The configured model when it is downloaded, else `small`, else any multilingual one.
    pub fn local_model(&self, cfg: &VoiceConfig) -> Option<String> {
        [cfg.model.as_str(), DEFAULT_MODEL]
            .into_iter()
            .find(|m| self.models.iter().any(|x| x == m))
            .map(str::to_string)
            .or_else(|| self.models.iter().find(|m| !m.ends_with(".en")).cloned())
            .or_else(|| self.models.first().cloned())
    }

    pub fn local_ready(&self, cfg: &VoiceConfig) -> bool {
        self.python.is_some() && self.ffmpeg.is_some() && self.local_model(cfg).is_some()
    }

    /// Engines to try, in order.
    pub fn engines(&self, cfg: &VoiceConfig) -> Vec<Engine> {
        let local = self.local_ready(cfg).then_some(Engine::Local);
        let remote = self.openai_key.is_some().then_some(Engine::Openai);
        match cfg.engine {
            Engine::Off => vec![],
            Engine::Local => local.into_iter().collect(),
            Engine::Openai => remote.into_iter().collect(),
            Engine::Auto => local.into_iter().chain(remote).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceError {
    Off,
    NotSetUp,
    TooLarge,
    Unsupported,
    NothingHeard,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub text: String,
    pub engine: Engine,
}

/// Tries each engine [`Setup::engines`] allows; a failing local run falls through to OpenAI.
pub fn transcribe(
    audio: &[u8],
    content_type: &str,
    cfg: &VoiceConfig,
    setup: &Setup,
) -> Result<Transcript, VoiceError> {
    if cfg.engine == Engine::Off {
        return Err(VoiceError::Off);
    }
    if audio.len() > MAX_AUDIO_BYTES {
        return Err(VoiceError::TooLarge);
    }
    let (extension, mime) = audio_format(content_type).ok_or(VoiceError::Unsupported)?;
    let engines = setup.engines(cfg);
    if engines.is_empty() {
        return Err(VoiceError::NotSetUp);
    }
    let mut last = VoiceError::NothingHeard;
    for engine in engines {
        let result = match engine {
            Engine::Local => local::transcribe(&local::Request {
                python: setup.python.as_deref().expect("local_ready"),
                ffmpeg: setup.ffmpeg.as_deref().expect("local_ready"),
                model: &setup.local_model(cfg).expect("local_ready"),
                language: &cfg.language,
                audio,
                extension,
            }),
            _ => openai::transcribe(&openai::Request {
                base: &setup.openai_base,
                key: setup.openai_key.as_deref().unwrap_or(""),
                language: &cfg.language,
                audio,
                extension,
                mime,
            }),
        };
        match result {
            Ok(text) if text.is_empty() => return Err(VoiceError::NothingHeard),
            Ok(text) => return Ok(Transcript { text, engine }),
            Err(e) => last = VoiceError::Failed(e),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::MemoryStore;

    #[test]
    fn a_missing_or_broken_file_gives_auto_small_and_detected_language() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("voice.json");
        assert_eq!(load(&p), VoiceConfig::default());
        std::fs::write(
            &p,
            r#"{"engine":"loud","model":"../x","language":"English","python":"py"}"#,
        )
        .unwrap();
        assert_eq!(load(&p), VoiceConfig::default());
    }

    #[test]
    fn saved_settings_load_back_and_other_keys_survive() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("voice.json");
        std::fs::write(&p, r#"{"keep":1}"#).unwrap();
        let cfg = VoiceConfig {
            engine: Engine::Openai,
            model: "large-v3".into(),
            language: "ja".into(),
            python: "/usr/bin/python3".into(),
        };
        save(&cfg, &p).unwrap();
        assert_eq!(load(&p), cfg);
        assert!(std::fs::read_to_string(&p).unwrap().contains("\"keep\": 1"));
        let bad = VoiceConfig {
            language: "Japanese".into(),
            ..cfg.clone()
        };
        assert!(save(&bad, &p).is_err());
        let relative = VoiceConfig {
            python: "python3".into(),
            ..cfg
        };
        assert!(save(&relative, &p).is_err());
    }

    #[test]
    fn the_stored_key_is_used() {
        let store = MemoryStore::new("loop-engineering.config");
        store.set(OPENAI_KEY_ACCOUNT, " sk-abc \n").unwrap();
        assert_eq!(openai_key(&store).as_deref(), Some("sk-abc"));
    }

    #[test]
    fn browser_and_app_recordings_map_to_upload_formats() {
        assert_eq!(
            audio_format("audio/webm;codecs=opus"),
            Some(("webm", "audio/webm"))
        );
        assert_eq!(audio_format("audio/mp4"), Some(("m4a", "audio/mp4")));
        assert_eq!(audio_format("Audio/X-M4A"), Some(("m4a", "audio/mp4")));
        assert_eq!(audio_format("text/plain"), None);
    }

    fn setup(models: &[&str]) -> Setup {
        Setup {
            python: Some("/py".into()),
            ffmpeg: Some("/ff".into()),
            models: models.iter().map(|s| s.to_string()).collect(),
            openai_key: None,
            openai_base: String::new(),
        }
    }

    #[test]
    fn the_configured_model_wins_then_small_then_any_multilingual_one() {
        let cfg = |m: &str| VoiceConfig {
            model: m.into(),
            ..Default::default()
        };
        let s = setup(&["large-v3", "medium.en", "small"]);
        assert_eq!(s.local_model(&cfg("large-v3")).as_deref(), Some("large-v3"));
        assert_eq!(s.local_model(&cfg("tiny")).as_deref(), Some("small"));
        assert_eq!(
            setup(&["medium.en", "large-v3"])
                .local_model(&cfg("tiny"))
                .as_deref(),
            Some("large-v3")
        );
        assert_eq!(
            setup(&["medium.en"]).local_model(&cfg("tiny")).as_deref(),
            Some("medium.en")
        );
        assert_eq!(setup(&[]).local_model(&cfg("small")), None);
    }

    #[test]
    fn auto_tries_local_then_openai_and_each_choice_only_itself() {
        let mut s = setup(&["small"]);
        s.openai_key = Some("k".into());
        let cfg = |engine| VoiceConfig {
            engine,
            ..Default::default()
        };
        assert_eq!(
            s.engines(&cfg(Engine::Auto)),
            vec![Engine::Local, Engine::Openai]
        );
        assert_eq!(s.engines(&cfg(Engine::Local)), vec![Engine::Local]);
        assert_eq!(s.engines(&cfg(Engine::Openai)), vec![Engine::Openai]);
        assert!(s.engines(&cfg(Engine::Off)).is_empty());
        s.ffmpeg = None;
        assert_eq!(s.engines(&cfg(Engine::Auto)), vec![Engine::Openai]);
        assert!(s.engines(&cfg(Engine::Local)).is_empty());
    }

    #[test]
    fn nothing_set_up_off_oversized_and_unknown_formats_are_told_apart() {
        let cfg = VoiceConfig::default();
        let none = Setup::default();
        assert_eq!(
            transcribe(b"x", "audio/webm", &cfg, &none),
            Err(VoiceError::NotSetUp)
        );
        let off = VoiceConfig {
            engine: Engine::Off,
            ..Default::default()
        };
        assert_eq!(
            transcribe(b"x", "audio/webm", &off, &none),
            Err(VoiceError::Off)
        );
        let big = vec![0u8; MAX_AUDIO_BYTES + 1];
        assert_eq!(
            transcribe(&big, "audio/webm", &cfg, &none),
            Err(VoiceError::TooLarge)
        );
        assert_eq!(
            transcribe(b"x", "text/html", &cfg, &none),
            Err(VoiceError::Unsupported)
        );
    }

    #[test]
    fn a_failing_local_run_falls_back_to_openai() {
        let d = tempfile::tempdir().unwrap();
        let mut s = setup(&["small"]);
        s.python = Some(local::tests::fake_python(d.path(), r#"{"error":"boom"}"#));
        s.ffmpeg = Some(local::tests::fake_ffmpeg(d.path()));
        let (base, _server) = openai::tests::one_shot(200, r#"{"text":"from the cloud"}"#);
        s.openai_key = Some("k".into());
        s.openai_base = base;
        let got = transcribe(b"x", "audio/webm", &VoiceConfig::default(), &s).unwrap();
        assert_eq!(
            got,
            Transcript {
                text: "from the cloud".into(),
                engine: Engine::Openai
            }
        );
    }

    #[test]
    fn local_only_reports_the_local_failure_and_silence_is_nothing_heard() {
        let d = tempfile::tempdir().unwrap();
        let mut s = setup(&["small"]);
        s.python = Some(local::tests::fake_python(d.path(), r#"{"error":"boom"}"#));
        s.ffmpeg = Some(local::tests::fake_ffmpeg(d.path()));
        let cfg = VoiceConfig {
            engine: Engine::Local,
            ..Default::default()
        };
        assert_eq!(
            transcribe(b"x", "audio/webm", &cfg, &s),
            Err(VoiceError::Failed("boom".into()))
        );
        let e = tempfile::tempdir().unwrap();
        s.python = Some(local::tests::fake_python(e.path(), r#"{"text":"  "}"#));
        assert_eq!(
            transcribe(b"x", "audio/webm", &cfg, &s),
            Err(VoiceError::NothingHeard)
        );
    }
}
