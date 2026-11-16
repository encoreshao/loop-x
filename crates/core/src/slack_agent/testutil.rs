//! Test helpers: a real local WebSocket server standing in for Slack.

use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;

use tungstenite::WebSocket;

/// Binds 127.0.0.1:0, accepts ONE websocket, runs `script(ws)` on a thread.
/// Returns the `ws://127.0.0.1:PORT` URL and the thread handle.
pub fn fake_socket_server(
    script: impl FnOnce(&mut WebSocket<TcpStream>) + Send + 'static,
) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
    let url = format!("ws://{}", listener.local_addr().expect("local addr"));
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut ws = tungstenite::accept(stream).expect("websocket handshake");
        script(&mut ws);
    });
    (url, handle)
}

/// One connection's script for [`fake_socket_server_seq`].
pub type Script = Box<dyn FnOnce(&mut WebSocket<TcpStream>) + Send>;

/// Like [`fake_socket_server`], but accepts one websocket per script, in
/// order, on the same URL (a client that reconnects gets the next script).
pub fn fake_socket_server_seq(scripts: Vec<Script>) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
    let url = format!("ws://{}", listener.local_addr().expect("local addr"));
    let handle = std::thread::spawn(move || {
        for script in scripts {
            let (stream, _) = listener.accept().expect("accept");
            let mut ws = tungstenite::accept(stream).expect("websocket handshake");
            script(&mut ws);
        }
    });
    (url, handle)
}
