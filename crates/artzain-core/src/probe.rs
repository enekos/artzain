//! Readiness/liveness probes. Deliberately dependency-free: a TCP connect and
//! a hand-rolled HTTP/1.1 `GET` over `tokio::net::TcpStream`. No reqwest/hyper
//! — the whole point of artzain is to be a small, predictable binary, and the
//! sutegi apps it supervises are themselves zero-dep.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// True if a TCP connection to `addr` (`host:port`) succeeds within 2s.
pub async fn tcp_once(addr: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// `GET path` against `addr` (`host:port`); true on a 2xx/3xx status line.
/// One connection, `Connection: close`, whole exchange bounded by `timeout`.
pub async fn http_once(addr: &str, path: &str, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, http_get(addr, path))
        .await
        .unwrap_or(false)
}

async fn http_get(addr: &str, path: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return false;
    };
    let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: artzain\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    // We only need the status line — read a small prefix of the response.
    let mut buf = [0u8; 256];
    let Ok(n) = stream.read(&mut buf).await else {
        return false;
    };
    status_ok(&buf[..n])
}

/// Parse `HTTP/1.1 200 OK` → accept 200..=399.
fn status_ok(head: &[u8]) -> bool {
    let line = match head.iter().position(|&b| b == b'\r' || b == b'\n') {
        Some(i) => &head[..i],
        None => head,
    };
    let text = String::from_utf8_lossy(line);
    let mut parts = text.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(proto), Some(code)) if proto.starts_with("HTTP/") => {
            matches!(code.parse::<u16>(), Ok(c) if (200..400).contains(&c))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::status_ok;

    #[test]
    fn accepts_2xx_3xx_rejects_others() {
        assert!(status_ok(b"HTTP/1.1 200 OK\r\n"));
        assert!(status_ok(b"HTTP/1.1 204 No Content\r\n"));
        assert!(status_ok(b"HTTP/1.1 301 Moved\r\n"));
        assert!(!status_ok(b"HTTP/1.1 404 Not Found\r\n"));
        assert!(!status_ok(b"HTTP/1.1 500 Boom\r\n"));
        assert!(!status_ok(b"garbage"));
        assert!(!status_ok(b""));
    }
}
