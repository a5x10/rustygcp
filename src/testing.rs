//! A one-shot HTTP server for the unit tests: answers the given replies in
//! order, one connection each, and keeps every request as it arrived on the
//! wire, so a test asserts on the real request rather than on a builder.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub struct Fake {
    /// `http://127.0.0.1:<port>`
    pub url: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    /// The requests so far: request line, headers, blank line, body.
    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    /// The JSON body of request `i`.
    pub fn body(&self, i: usize) -> serde_json::Value {
        let seen = self.seen();
        let (_, body) = seen[i].split_once("\r\n\r\n").expect("a body");
        serde_json::from_str(body).expect("a JSON body")
    }
}

/// `(status, JSON body)` per expected request.
pub async fn fake(replies: Vec<(u16, String)>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for (status, body) in replies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some((head, got)) = text.split_once("\r\n\r\n") {
                    let want = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")?
                                .parse()
                                .ok()
                        })
                        .unwrap_or(0usize);
                    if got.len() >= want {
                        break;
                    }
                }
                assert!(n > 0, "the client hung up mid-request");
            }
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf).into_owned());
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(reply.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
        }
    });
    Fake { url, seen }
}
