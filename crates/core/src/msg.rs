//! A user-facing result message that is translated by whoever renders it (the
//! dashboard turns `Tr` into the request language; the CLI prints English).
//! Mirrors the Python helpers returning `(ok, _t("..."))`.

use crate::pyfmt_lite::format_named;

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// An English template with `{name}` placeholders and its arguments.
    Tr(String, Vec<(String, String)>),
    /// Text that must not be translated or formatted (a subprocess's stderr).
    Raw(String),
}

impl Msg {
    pub fn tr(text: &str) -> Msg {
        Msg::Tr(text.to_string(), Vec::new())
    }

    pub fn tra(text: &str, args: &[(&str, &str)]) -> Msg {
        Msg::Tr(
            text.to_string(),
            args.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    pub fn raw(text: impl Into<String>) -> Msg {
        Msg::Raw(text.into())
    }

    /// The English text.
    pub fn english(&self) -> String {
        match self {
            Msg::Tr(text, args) => {
                let refs: Vec<(&str, &str)> =
                    args.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                format_named(text, &refs)
            }
            Msg::Raw(s) => s.clone(),
        }
    }
}

/// `(ok, message)`.
pub type Outcome = (bool, Msg);

pub fn ok(msg: Msg) -> Outcome {
    (true, msg)
}

pub fn fail(msg: Msg) -> Outcome {
    (false, msg)
}
