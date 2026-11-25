//! Voice input's user-facing text: transcription errors and the Settings › Voice status line.

use super::i18n::{self, Lang};
use super::state::UiState;
use loopx_core::keychain::{self, MemoryStore, SecretStore, SERVICE_CONFIG};
use loopx_core::voice::{Engine, Setup, VoiceConfig, VoiceError};
use std::sync::Arc;

pub fn secret_store(state: &UiState) -> Arc<dyn SecretStore> {
    match &state.voice_store {
        Some(s) => s.clone(),
        None => match keychain::open_store(SERVICE_CONFIG) {
            Ok(s) => Arc::from(s),
            Err(_) => Arc::new(MemoryStore::new(SERVICE_CONFIG)),
        },
    }
}

pub fn error_text(lang: Lang, err: &VoiceError) -> String {
    let t = |s: &str| i18n::t(lang, s, &[]);
    match err {
        VoiceError::Off => t("Voice input is turned off in Settings."),
        VoiceError::NotSetUp => t("Voice input isn't set up yet. Install faster-whisper and ffmpeg, or add an OpenAI API key in Settings."),
        VoiceError::TooLarge => t("The recording is too long. Keep it under a few minutes."),
        VoiceError::Unsupported => t("This recording format isn't supported."),
        VoiceError::NothingHeard => t("Nothing was heard. Try again a little closer to the microphone."),
        VoiceError::Failed(detail) => i18n::t(lang, "Couldn't transcribe the recording: {detail}", &[("detail", detail)]),
    }
}

/// One line saying which engine a recording would go to, and why not when none.
pub fn status_text(lang: Lang, cfg: &VoiceConfig, setup: &Setup) -> (bool, String) {
    let engines = setup.engines(cfg);
    match engines.first() {
        Some(Engine::Local) => {
            let model = setup.local_model(cfg).unwrap_or_default();
            let python = setup
                .python
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let mut text = i18n::t(
                lang,
                "Using local Whisper ({model}) with {python}.",
                &[("model", &model), ("python", &python)],
            );
            if engines.contains(&Engine::Openai) {
                text.push(' ');
                text.push_str(&i18n::t(lang, "OpenAI is the fallback.", &[]));
            }
            (true, text)
        }
        Some(_) => (true, i18n::t(lang, "Using the OpenAI API.", &[])),
        None if cfg.engine == Engine::Off => (false, i18n::t(lang, "Voice input is off.", &[])),
        None => {
            let mut missing = Vec::new();
            if matches!(cfg.engine, Engine::Auto | Engine::Local) {
                if setup.python.is_none() {
                    missing.push(i18n::t(lang, "no Python with faster-whisper found", &[]));
                }
                if setup.ffmpeg.is_none() {
                    missing.push(i18n::t(lang, "ffmpeg not found", &[]));
                }
                if setup.models.is_empty() {
                    missing.push(i18n::t(lang, "no Whisper model downloaded", &[]));
                }
            }
            if matches!(cfg.engine, Engine::Auto | Engine::Openai) && setup.openai_key.is_none() {
                missing.push(i18n::t(lang, "no OpenAI API key", &[]));
            }
            (
                false,
                i18n::t(
                    lang,
                    "Not ready: {reasons}.",
                    &[("reasons", &missing.join(", "))],
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Setup {
        Setup {
            python: Some("/py/bin/python3".into()),
            ffmpeg: Some("/bin/ffmpeg".into()),
            models: vec!["small".into()],
            openai_key: None,
            openai_base: String::new(),
        }
    }

    #[test]
    fn the_status_names_the_engine_or_what_is_missing() {
        let cfg = VoiceConfig::default();
        let (ok, text) = status_text(Lang::En, &cfg, &ready());
        assert!(ok);
        assert_eq!(text, "Using local Whisper (small) with /py/bin/python3.");
        let (ok, text) = status_text(Lang::En, &cfg, &Setup::default());
        assert!(!ok);
        assert_eq!(
            text,
            "Not ready: no Python with faster-whisper found, ffmpeg not found, no Whisper model downloaded, no OpenAI API key."
        );
        let openai = VoiceConfig {
            engine: Engine::Openai,
            ..Default::default()
        };
        assert_eq!(
            status_text(Lang::En, &openai, &ready()).1,
            "Not ready: no OpenAI API key."
        );
    }

    #[test]
    fn errors_are_translated() {
        assert_ne!(
            error_text(Lang::Fr, &VoiceError::NothingHeard),
            error_text(Lang::En, &VoiceError::NothingHeard)
        );
        assert!(error_text(Lang::En, &VoiceError::Failed("boom".into())).ends_with(": boom"));
    }
}
