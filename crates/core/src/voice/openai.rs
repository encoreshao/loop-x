//! OpenAI's speech-to-text endpoint (`POST /v1/audio/transcriptions`).

use std::time::Duration;

pub const DEFAULT_BASE: &str = "https://api.openai.com/v1";
pub const MODEL: &str = "whisper-1";
const TIMEOUT: Duration = Duration::from_secs(120);

pub struct Request<'a> {
    pub base: &'a str,
    pub key: &'a str,
    pub language: &'a str,
    pub audio: &'a [u8],
    pub extension: &'a str,
    pub mime: &'a str,
}

fn multipart(req: &Request, boundary: &str) -> Vec<u8> {
    let mut body = Vec::new();
    let mut field = |name: &str, value: &str| {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    };
    field("model", MODEL);
    field("response_format", "json");
    if !req.language.is_empty() {
        field("language", req.language);
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.{}\"\r\nContent-Type: {}\r\n\r\n",
            req.extension, req.mime
        )
        .as_bytes(),
    );
    body.extend_from_slice(req.audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn error_message(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.pointer("/error/message")?.as_str().map(str::to_string)
}

pub fn transcribe(req: &Request) -> Result<String, String> {
    let boundary = format!("loopx-{}", uuid::Uuid::new_v4().simple());
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let resp = agent
        .post(&format!(
            "{}/audio/transcriptions",
            req.base.trim_end_matches('/')
        ))
        .set("Authorization", &format!("Bearer {}", req.key))
        .set(
            "Content-Type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .send_bytes(&multipart(req, &boundary));
    match resp {
        Ok(r) => {
            let body = r.into_string().map_err(|e| e.to_string())?;
            let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            Ok(v.get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .trim()
                .to_string())
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            Err(format!(
                "OpenAI HTTP {code}: {}",
                error_message(&body).unwrap_or(body)
            ))
        }
        Err(e) => Err(format!("could not reach OpenAI: {e}")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serves one request with `status` and `body`; the thread returns what it received.
    pub(crate) fn one_shot(
        status: u16,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got);
                if let Some(end) = text.find("\r\n\r\n") {
                    let len: usize = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if got.len() >= end + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            write!(s, "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            got
        });
        (base, handle)
    }

    fn request<'a>(base: &'a str, language: &'a str) -> Request<'a> {
        Request {
            base,
            key: "sk-test",
            language,
            audio: b"OPUSDATA",
            extension: "webm",
            mime: "audio/webm",
        }
    }

    #[test]
    fn sends_the_key_model_language_and_file_and_returns_the_text() {
        let (base, server) = one_shot(200, r#"{"text":" Hola mundo "}"#);
        assert_eq!(transcribe(&request(&base, "es")).unwrap(), "Hola mundo");
        let got = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
        assert!(got.starts_with("POST /v1/audio/transcriptions "), "{got}");
        assert!(got.contains("Bearer sk-test"));
        assert!(got.contains("name=\"model\"\r\n\r\nwhisper-1\r\n"));
        assert!(got.contains("name=\"language\"\r\n\r\nes\r\n"));
        assert!(
            got.contains("filename=\"audio.webm\"\r\nContent-Type: audio/webm\r\n\r\nOPUSDATA\r\n")
        );
    }

    #[test]
    fn auto_language_sends_no_language_field() {
        let (base, server) = one_shot(200, r#"{"text":"hi"}"#);
        transcribe(&request(&base, "")).unwrap();
        let got = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
        assert!(!got.contains("name=\"language\""));
    }

    #[test]
    fn an_api_error_carries_openais_message() {
        let (base, _server) =
            one_shot(401, r#"{"error":{"message":"Incorrect API key provided"}}"#);
        let err = transcribe(&request(&base, "")).unwrap_err();
        assert_eq!(err, "OpenAI HTTP 401: Incorrect API key provided");
    }
}
