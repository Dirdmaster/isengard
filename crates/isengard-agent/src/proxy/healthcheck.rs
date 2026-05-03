//! Per-upstream healthcheck primitive.
//!
//! Two modes:
//! - HTTP: GET <path>, 2xx response = healthy
//! - TCP-only: connect succeeds = healthy
//!
//! Used by the eviction state machine (Task 19) which calls `check_once`
//! on a tick and updates `Upstream.healthy` based on the result.

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

pub struct HealthChecker {
    path: Option<String>,
    timeout: Duration,
}

impl HealthChecker {
    pub fn new(path: String, timeout: Duration) -> Self {
        Self {
            path: Some(path),
            timeout,
        }
    }

    pub fn tcp_only(timeout: Duration) -> Self {
        Self {
            path: None,
            timeout,
        }
    }

    pub async fn check_once(&self, addr: SocketAddr) -> bool {
        let connect = timeout(self.timeout, TcpStream::connect(addr)).await;
        let mut stream = match connect {
            Ok(Ok(s)) => s,
            _ => return false,
        };
        let Some(path) = &self.path else {
            return true;
        };
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: healthcheck\r\nConnection: close\r\n\r\n",
            path
        );
        if timeout(self.timeout, stream.write_all(req.as_bytes()))
            .await
            .is_err()
        {
            return false;
        }
        let mut buf = vec![0u8; 256];
        let n = match timeout(self.timeout, stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => return false,
        };
        let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        (200..300).contains(&status)
    }
}
