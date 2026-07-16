//! A fault-controllable HTTP app for artzain integration tests.
//!
//! Serves the sutegi-shaped probe endpoints `/__ready`, `/__health`, and
//! `/__metrics`. Behavior is scripted through environment variables so the
//! test driver can inject failures without modifying the source:
//!
//! - `FIXTURE_READY_AFTER_MS`   — `/__ready` returns 503 until this uptime.
//! - `FIXTURE_LIVE_FAIL_AFTER_MS` — `/__health` returns 500 after this uptime.
//! - `FIXTURE_CRASH_AFTER_MS`   — exit with `FIXTURE_EXIT_CODE` after this uptime.
//! - `FIXTURE_LOG_EVERY_MS`    — print a line to stdout every N ms.
//!
//! The fixture also exposes `/__env` so tests can assert on the environment
//! artzain injected, `/__id` to assert on uid/gid after a privilege drop, and
//! `/__limits` to assert on applied `RLIMIT_NOFILE` / `RLIMIT_AS`.

use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let port = std::env::var("PORT")
        .expect("PORT env required")
        .parse::<u16>()
        .expect("PORT must be a u16");

    let start = Instant::now();
    let ready_after = parse_ms("FIXTURE_READY_AFTER_MS");
    let live_fail_after = parse_ms("FIXTURE_LIVE_FAIL_AFTER_MS");
    let crash_after = parse_ms("FIXTURE_CRASH_AFTER_MS");
    let exit_code = std::env::var("FIXTURE_EXIT_CODE")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);
    let ignore_sigterm = std::env::var("FIXTURE_IGNORE_SIGTERM").is_ok();
    let log_every = parse_ms("FIXTURE_LOG_EVERY_MS");

    if ignore_sigterm {
        #[cfg(unix)]
        {
            // Installing a tokio signal handler replaces the default SIGTERM
            // action with a no-op until the process exits some other way.
            let _ = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        }
    }

    if let Some(after) = crash_after {
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            std::process::exit(exit_code);
        });
    }

    if let Some(period) = log_every {
        let millis = period.as_millis() as u64;
        std::thread::spawn(move || {
            let mut counter = 0u64;
            loop {
                std::thread::sleep(Duration::from_millis(millis));
                counter += 1;
                println!("fixture log line {counter} padding to make a longer record");
            }
        });
    }

    let listener = match TcpListener::bind(format!("127.0.0.1:{port}")).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("fixture could not bind port {port}: {e}");
            std::process::exit(1);
        }
    };

    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(c) => c,
            Err(_) => continue,
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n = match stream.read(&mut buf).await {
                Ok(n) if n > 0 => n,
                _ => return,
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let first = req.lines().next().unwrap_or("");
            let path = first.split_whitespace().nth(1).unwrap_or("/");

            let uptime = start.elapsed();
            let (status, body): (&str, String) = match path {
                "/__ready" => {
                    if ready_after.map(|d| uptime >= d).unwrap_or(true) {
                        ("200 OK", "ready".to_string())
                    } else {
                        ("503 Not Ready", "not ready".to_string())
                    }
                }
                "/__health" => {
                    if live_fail_after.map(|d| uptime >= d).unwrap_or(false) {
                        ("500 Internal Server Error", "unhealthy".to_string())
                    } else {
                        ("200 OK", "healthy".to_string())
                    }
                }
                "/__metrics" => ("200 OK", "ok".to_string()),
                "/__env" => ("200 OK", env_json()),
                "/__id" => ("200 OK", id_json()),
                "/__limits" => ("200 OK", limits_json()),
                _ => ("404 Not Found", "not found".to_string()),
            };

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

fn parse_ms(var: &str) -> Option<Duration> {
    std::env::var(var)
        .ok()?
        .parse::<u64>()
        .ok()
        .map(Duration::from_millis)
}

fn env_json() -> String {
    json_object(std::env::vars().collect())
}

#[cfg(unix)]
fn id_json() -> String {
    format!("uid={}\ngid={}", unsafe { libc::getuid() }, unsafe {
        libc::getgid()
    })
}

#[cfg(not(unix))]
fn id_json() -> String {
    "uid=0\ngid=0".to_string()
}

fn limits_json() -> String {
    let open_files = read_proc_limit("Max open files");
    let address_space = read_proc_limit("Max address space");
    format!("open_files={}\naddress_space={}", open_files, address_space)
}

fn read_proc_limit(label: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/self/limits") {
            for line in text.lines() {
                if line.starts_with(label) {
                    // Lines look like: "Max open files  1024  1024  files"
                    let cols: Vec<_> = line.split_whitespace().collect();
                    if let Some(val) = cols.get(3) {
                        return val.to_string();
                    }
                }
            }
        }
    }
    let _ = label;
    String::new()
}

fn json_object(items: Vec<(String, String)>) -> String {
    let mut out = String::from("{");
    let mut first = true;
    for (key, value) in items {
        if !first {
            out.push_str(", ");
        }
        first = false;
        out.push('"');
        out.push_str(&key);
        out.push_str("\": \"");
        out.push_str(&value);
        out.push('"');
    }
    out.push('}');
    out
}
