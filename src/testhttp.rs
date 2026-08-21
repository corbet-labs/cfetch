//! A minimal canned-response HTTP server for tests, std `TcpListener` only.
//!
//! Every outbound HTTP client in this crate — embeddings, reranking — needs
//! the same thing from its tests: drive a real socket, record exactly what was
//! sent, and hand back a response the test chose byte for byte. Sharing one
//! harness keeps those clients honest against the SAME server rather than
//! against two subtly different fakes.

use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};

pub(crate) fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Reads one HTTP request; returns (headers, body).
pub(crate) fn read_request(s: &mut std::net::TcpStream) -> Option<(String, String)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        let n = s.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = headers
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = s.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body =
        String::from_utf8_lossy(&buf[header_end..(header_end + content_length).min(buf.len())]).to_string();
    Some((headers, body))
}

pub(crate) fn http_response(status: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Spawns a one-connection-at-a-time server; `responder(request_no,
/// request_body)` produces the FULL http response. Returns (base_url,
/// recorded request bodies, recorded request headers).
#[allow(clippy::type_complexity)]
pub(crate) fn spawn_server<F>(responder: F) -> (String, Arc<Mutex<Vec<String>>>, Arc<Mutex<Vec<String>>>)
where
    F: Fn(usize, &str) -> String + Send + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let headers = Arc::new(Mutex::new(Vec::new()));
    let (recorded_bodies, recorded_headers) = (bodies.clone(), headers.clone());
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().enumerate() {
            let Ok(mut s) = stream else { break };
            let Some((hdrs, body)) = read_request(&mut s) else { continue };
            recorded_bodies.lock().unwrap().push(body.clone());
            recorded_headers.lock().unwrap().push(hdrs);
            let _ = s.write_all(responder(n, &body).as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}"), bodies, headers)
}
