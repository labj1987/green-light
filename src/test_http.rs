//! A tiny HTTP/1.1 server for tests: canned responses by request path (and,
//! optionally, a piece of the query string).

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct Route {
    /// Path without the query string.
    pub path: &'static str,
    /// Text the query string must contain ("" matches any).
    pub query: &'static str,
    pub status: u16,
    pub body: Vec<u8>,
    /// Content-Length to announce; more than the body simulates a server
    /// that closes the connection early.
    pub announce_len: Option<usize>,
}

pub fn route(path: &'static str, status: u16, body: &[u8]) -> Route {
    Route { path, query: "", status, body: body.to_vec(), announce_len: None }
}

/// Serve `routes` on a free local port (anything else is a 404) and return
/// the base URL, ending in `/`.
pub async fn serve(routes: Vec<Route>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req);
                let target = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                let r = routes
                    .iter()
                    .find(|r| r.path == path && query.contains(r.query))
                    .cloned()
                    .unwrap_or_else(|| route("", 404, b"Not found"));
                let head = format!(
                    "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    r.status,
                    r.announce_len.unwrap_or(r.body.len())
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&r.body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}/")
}

/// A base URL nothing listens on.
pub async fn dead_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/")
}
