//! Slack Socket Mode client: parses envelopes, acknowledges them and reconnects
//! with a deterministic backoff (the caller adds jitter).

use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::http::Uri;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Error as WsError, HandshakeError, Message, WebSocket};

/// What the client hands to the loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Hello,
    Disconnect {
        reason: String,
    },
    /// `events_api` envelope.
    Event {
        envelope_id: String,
        payload: Value,
    },
    /// `interactive` envelope (block_actions).
    Interactive {
        envelope_id: String,
        payload: Value,
    },
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SocketError {
    #[error("socket closed")]
    Closed,
    #[error("socket io: {0}")]
    Io(String),
    #[error("socket protocol: {0}")]
    Protocol(String),
}

/// `None` means the text is not a Socket Mode frame at all.
pub fn parse_frame(text: &str) -> Option<Frame> {
    let v: Value = serde_json::from_str(text).ok()?;
    let kind = v.get("type")?.as_str()?;
    let envelope = |v: &Value| -> Option<(String, Value)> {
        let id = v.get("envelope_id")?.as_str()?.to_string();
        let payload = v.get("payload").cloned().unwrap_or(Value::Null);
        Some((id, payload))
    };
    Some(match kind {
        "hello" => Frame::Hello,
        "disconnect" => Frame::Disconnect {
            reason: v
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        "events_api" => {
            let (envelope_id, payload) = envelope(&v)?;
            Frame::Event {
                envelope_id,
                payload,
            }
        }
        "interactive" => {
            let (envelope_id, payload) = envelope(&v)?;
            Frame::Interactive {
                envelope_id,
                payload,
            }
        }
        _ => Frame::Other,
    })
}

/// The acknowledgement Slack expects for an envelope.
pub fn ack_message(envelope_id: &str) -> String {
    serde_json::json!({ "envelope_id": envelope_id }).to_string()
}

/// 1s, 2s, 4s ... capped at 60s.
pub fn backoff(attempt: u32) -> Duration {
    let secs = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    Duration::from_secs(secs.min(60))
}

fn io_error(e: WsError) -> SocketError {
    match e {
        WsError::ConnectionClosed | WsError::AlreadyClosed => SocketError::Closed,
        WsError::Io(e) => SocketError::Io(e.to_string()),
        other => SocketError::Protocol(other.to_string()),
    }
}

/// A connected Socket Mode websocket (blocking, one reader).
pub struct SocketClient {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    /// When anything last arrived (data or control frame). A socket that
    /// survived a laptop sleep looks `ESTABLISHED` yet is dead, and only
    /// silence reveals it.
    last_rx: Instant,
}

/// Bound on the TCP connect and on each read/write of the websocket handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Connect errors never carry the URL: a Socket Mode URL embeds a `ticket`
/// credential and tungstenite error text can echo the URI.
fn connect_error(e: WsError) -> SocketError {
    match e {
        WsError::Url(_) => SocketError::Protocol("invalid websocket url".into()),
        WsError::Http(resp) => {
            SocketError::Protocol(format!("handshake rejected: HTTP {}", resp.status()))
        }
        WsError::Io(e) => SocketError::Io(e.to_string()),
        WsError::ConnectionClosed | WsError::AlreadyClosed => SocketError::Closed,
        WsError::Tls(_) => SocketError::Protocol("tls handshake failed".into()),
        _ => SocketError::Protocol("websocket handshake failed".into()),
    }
}

impl SocketClient {
    pub fn connect(url: &str) -> Result<Self, SocketError> {
        Self::connect_with_timeout(url, CONNECT_TIMEOUT)
    }

    fn connect_with_timeout(url: &str, timeout: Duration) -> Result<Self, SocketError> {
        let bad_url = || SocketError::Protocol("invalid websocket url".into());
        let uri: Uri = url.parse().map_err(|_| bad_url())?;
        let host = uri.host().ok_or_else(bad_url)?;
        let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
            Some("wss") => 443,
            Some("ws") => 80,
            _ => return Err(bad_url()),
        });
        let mut last = SocketError::Io("no address resolved".into());
        let mut stream = None;
        for addr in (host, port)
            .to_socket_addrs()
            .map_err(|e| SocketError::Io(e.to_string()))?
        {
            match TcpStream::connect_timeout(&addr, timeout) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last = SocketError::Io(e.to_string()),
            }
        }
        let stream = stream.ok_or(last)?;
        // Bound every handshake read/write (TLS included); `next()` resets the read side.
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|_| stream.set_write_timeout(Some(timeout)))
            .map_err(|e| SocketError::Io(e.to_string()))?;
        match tungstenite::client_tls(url, stream) {
            Ok((ws, _)) => Ok(Self {
                ws,
                last_rx: Instant::now(),
            }),
            Err(HandshakeError::Failure(e)) => Err(connect_error(e)),
            Err(HandshakeError::Interrupted(_)) => {
                Err(SocketError::Io("websocket handshake timed out".into()))
            }
        }
    }

    /// How long since the last frame of any kind arrived.
    pub fn idle_for(&self) -> Duration {
        self.last_rx.elapsed()
    }

    fn tcp(&self) -> Option<&TcpStream> {
        match self.ws.get_ref() {
            MaybeTlsStream::Plain(s) => Some(s),
            MaybeTlsStream::Rustls(s) => Some(s.get_ref()),
            _ => None,
        }
    }

    /// Blocks up to `timeout` in total (pings and other control frames do not
    /// extend it); `Ok(None)` on timeout.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Frame>, SocketError> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            let tcp = self
                .tcp()
                .ok_or_else(|| SocketError::Protocol("unsupported stream".into()))?;
            // A zero timeout means "block forever" to the OS; never pass one.
            tcp.set_read_timeout(Some(left.max(Duration::from_millis(1))))
                .map_err(|e| SocketError::Io(e.to_string()))?;
            let read = self.ws.read();
            if read.is_ok() {
                self.last_rx = Instant::now();
            }
            match read {
                Ok(Message::Text(t)) => {
                    return Ok(Some(parse_frame(&t).unwrap_or(Frame::Other)));
                }
                Ok(Message::Ping(_)) => {
                    // tungstenite queued the Pong; push it out now.
                    match self.ws.flush() {
                        Ok(()) => {}
                        Err(WsError::Io(e)) if is_timeout(&e) => {}
                        Err(e) => return Err(io_error(e)),
                    }
                }
                Ok(Message::Close(_)) => {
                    // Complete the close handshake (best effort) before reporting it.
                    let _ = self.ws.flush();
                    return Err(SocketError::Closed);
                }
                Ok(_) => {}
                Err(WsError::Io(e)) if is_timeout(&e) => return Ok(None),
                Err(e) => return Err(io_error(e)),
            }
        }
    }

    pub fn ack(&mut self, envelope_id: &str) -> Result<(), SocketError> {
        self.ws
            .send(Message::Text(ack_message(envelope_id)))
            .map_err(io_error)
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_api_envelope() {
        let t = r#"{"type":"events_api","envelope_id":"e1","payload":{"event":{"type":"app_mention"}}}"#;
        match parse_frame(t) {
            Some(Frame::Event {
                envelope_id,
                payload,
            }) => {
                assert_eq!(envelope_id, "e1");
                assert_eq!(payload["event"]["type"], "app_mention");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_interactive_envelope() {
        let t = r#"{"type":"interactive","envelope_id":"i1","payload":{"type":"block_actions"}}"#;
        match parse_frame(t) {
            Some(Frame::Interactive {
                envelope_id,
                payload,
            }) => {
                assert_eq!(envelope_id, "i1");
                assert_eq!(payload["type"], "block_actions");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_hello_and_disconnect() {
        assert!(matches!(
            parse_frame(r#"{"type":"hello"}"#),
            Some(Frame::Hello)
        ));
        match parse_frame(r#"{"type":"disconnect","reason":"refresh_requested"}"#) {
            Some(Frame::Disconnect { reason }) => assert_eq!(reason, "refresh_requested"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_type_is_other_and_garbage_is_none() {
        assert!(matches!(
            parse_frame(r#"{"type":"slash_commands","envelope_id":"x"}"#),
            Some(Frame::Other)
        ));
        assert!(parse_frame("not json").is_none());
        assert!(parse_frame(r#"{"no_type":1}"#).is_none());
        assert!(parse_frame(r#"{"type":"events_api"}"#).is_none());
    }

    #[test]
    fn ack_message_is_envelope_json() {
        assert_eq!(ack_message("e1"), r#"{"envelope_id":"e1"}"#);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(0).as_secs(), 1);
        assert_eq!(backoff(3).as_secs(), 8);
        assert_eq!(backoff(20).as_secs(), 60);
        assert_eq!(backoff(u32::MAX).as_secs(), 60);
    }

    use crate::slack_agent::testutil::fake_socket_server;
    use tungstenite::Message;

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn client_round_trip_against_a_real_server() {
        let (url, server) = fake_socket_server(|ws| {
            ws.send(Message::Text(r#"{"type":"hello"}"#.into()))
                .unwrap();
            ws.send(Message::text(
                r#"{"type":"events_api","envelope_id":"e1","payload":{"event":{"type":"app_mention"}}}"#,
            ))
            .unwrap();
            match ws.read().unwrap() {
                Message::Text(t) => assert_eq!(t, r#"{"envelope_id":"e1"}"#),
                other => panic!("{other:?}"),
            }
        });
        let mut client = SocketClient::connect(&url).unwrap();
        assert_eq!(client.next(WAIT).unwrap(), Some(Frame::Hello));
        match client.next(WAIT).unwrap() {
            Some(Frame::Event { envelope_id, .. }) => {
                assert_eq!(envelope_id, "e1");
                client.ack(&envelope_id).unwrap();
            }
            other => panic!("{other:?}"),
        }
        server.join().unwrap();
    }

    #[test]
    fn next_times_out_with_none_when_the_server_is_quiet() {
        let (url, server) = fake_socket_server(|ws| {
            // Hold the connection open until the client goes away.
            let _ = ws.read();
        });
        let mut client = SocketClient::connect(&url).unwrap();
        assert_eq!(client.next(Duration::from_millis(100)).unwrap(), None);
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn malformed_frame_is_other_then_close_is_closed() {
        let (url, server) = fake_socket_server(|ws| {
            ws.send(Message::Text("}{".into())).unwrap();
            ws.close(None).unwrap();
            while ws.read().is_ok() {}
        });
        let mut client = SocketClient::connect(&url).unwrap();
        assert_eq!(client.next(WAIT).unwrap(), Some(Frame::Other));
        assert_eq!(client.next(WAIT), Err(SocketError::Closed));
        server.join().unwrap();
    }

    #[test]
    fn ping_is_answered_with_pong() {
        let (url, server) = fake_socket_server(|ws| {
            ws.send(Message::Ping(vec![1, 2])).unwrap();
            ws.send(Message::Text(r#"{"type":"hello"}"#.into()))
                .unwrap();
            match ws.read().unwrap() {
                Message::Pong(p) => assert_eq!(p, vec![1, 2]),
                other => panic!("{other:?}"),
            }
        });
        let mut client = SocketClient::connect(&url).unwrap();
        assert_eq!(client.next(WAIT).unwrap(), Some(Frame::Hello));
        server.join().unwrap();
    }

    #[test]
    fn next_returns_by_its_timeout_even_while_the_server_keeps_pinging() {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let (url, server) = fake_socket_server(move |ws| {
            // A ping every 20 ms: each read returns well inside the timeout.
            while done_rx.try_recv().is_err() {
                if ws.send(Message::Ping(vec![9])).is_err() {
                    return;
                }
                let _ = ws.flush();
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let mut client = SocketClient::connect(&url).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let r = client.next(Duration::from_millis(300));
            let _ = tx.send((r, started.elapsed()));
            client
        });
        let (r, took) = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("next() never returned while pings kept arriving");
        assert_eq!(r, Ok(None));
        assert!(took < Duration::from_secs(2), "{took:?}");
        done_tx.send(()).unwrap();
        drop(reader.join().unwrap());
        server.join().unwrap();
    }

    #[test]
    fn idle_for_counts_silence_and_resets_on_any_frame_even_a_ping() {
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let (pong_tx, pong_rx) = std::sync::mpsc::channel::<()>();
        let (url, server) = fake_socket_server(move |ws| {
            let _ = go_rx.recv();
            ws.send(Message::Ping(vec![1])).unwrap();
            let _ = ws.flush();
            while let Ok(msg) = ws.read() {
                if matches!(msg, Message::Pong(_)) {
                    let _ = pong_tx.send(());
                }
            }
        });
        let mut client = SocketClient::connect(&url).unwrap();
        assert_eq!(client.next(Duration::from_millis(300)).unwrap(), None);
        assert!(
            client.idle_for() >= Duration::from_millis(300),
            "{:?}",
            client.idle_for()
        );
        go_tx.send(()).unwrap();
        let pinged_after = Instant::now();
        // The client answers the ping only after reading it, so a pong means the clock was reset.
        while pong_rx.try_recv().is_err() {
            assert!(pinged_after.elapsed() < Duration::from_secs(10), "no pong");
            assert_eq!(client.next(Duration::from_millis(20)).unwrap(), None);
        }
        assert!(
            client.idle_for() < pinged_after.elapsed(),
            "{:?} not reset by the ping",
            client.idle_for()
        );
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn connect_errors_never_leak_the_url_ticket() {
        for url in [
            "ws://127.0.0.1:1/?ticket=SECRET",
            "ftp://127.0.0.1:1/?ticket=SECRET",
            "not a url ticket=SECRET",
        ] {
            let err = match SocketClient::connect(url) {
                Err(e) => e,
                Ok(_) => panic!("connect to {url} unexpectedly succeeded"),
            };
            assert!(!err.to_string().contains("SECRET"), "{err}");
            assert!(!format!("{err:?}").contains("SECRET"), "{err:?}");
        }
    }

    #[test]
    fn rejected_handshake_reports_status_without_the_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            s.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n")
                .unwrap();
        });
        let err = match SocketClient::connect(&format!("ws://127.0.0.1:{port}/?ticket=SECRET")) {
            Err(e) => e,
            Ok(_) => panic!("handshake unexpectedly succeeded"),
        };
        assert!(err.to_string().contains("403"), "{err}");
        assert!(!format!("{err} {err:?}").contains("SECRET"));
        server.join().unwrap();
    }

    #[test]
    fn stalled_handshake_times_out_without_leaking_the_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept the TCP connection but never answer the websocket handshake.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (_s, _) = listener.accept().unwrap();
            let _ = done_rx.recv();
        });
        let started = std::time::Instant::now();
        let err = match SocketClient::connect_with_timeout(
            &format!("ws://127.0.0.1:{port}/?ticket=SECRET"),
            Duration::from_millis(300),
        ) {
            Err(e) => e,
            Ok(_) => panic!("handshake unexpectedly succeeded"),
        };
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(err, SocketError::Io(_)), "{err:?}");
        assert!(!format!("{err} {err:?}").contains("SECRET"));
        done_tx.send(()).unwrap();
        server.join().unwrap();
    }
}
