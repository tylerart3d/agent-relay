#![cfg(any(windows, all(target_os = "macos", target_arch = "aarch64")))]

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    child: Child,
    endpoint: String,
    stop: Arc<AtomicBool>,
    config_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(mut socket) = TcpStream::connect(self.endpoint.trim_start_matches("http://")) {
            let _ = socket
                .write_all(b"GET /unload HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stop.store(true, Ordering::SeqCst);
        let _ = fs::remove_dir_all(&self.config_dir);
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve port")
        .local_addr()
        .expect("reserved address")
        .port()
}

fn sidecar_path() -> PathBuf {
    let name = if cfg!(windows) {
        "llama-swap-x86_64-pc-windows-msvc.exe"
    } else {
        "llama-swap-aarch64-apple-darwin"
    };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("binaries")
        .join(name)
}

fn sleeper_command() -> &'static str {
    if cfg!(windows) {
        "powershell.exe -NoProfile -Command Start-Sleep -Seconds 60"
    } else {
        "/bin/sh -c 'sleep 60'"
    }
}

fn start_fake_upstream(stream_seconds: u64) -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake upstream");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("upstream address")
    );
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    thread::spawn(move || {
        while !server_stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    thread::spawn(move || {
                        let mut request = [0_u8; 8192];
                        let read = socket.read(&mut request).unwrap_or_default();
                        let request = String::from_utf8_lossy(&request[..read]);
                        if request.contains("/v1/chat/completions") {
                            socket
                                .write_all(
                                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                                )
                                .expect("stream headers");
                            socket
                                .write_all(b"10\r\ndata: {\"part\":1}\n\n\r\n")
                                .expect("first stream chunk");
                            socket.flush().expect("flush first chunk");
                            thread::sleep(Duration::from_secs(stream_seconds));
                            socket
                                .write_all(b"E\r\ndata: [DONE]\n\n\r\n0\r\n\r\n")
                                .expect("final stream chunk");
                        } else {
                            let body = b"{\"data\":[]}";
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                                body.len()
                            );
                            socket
                                .write_all(response.as_bytes())
                                .expect("health headers");
                            socket.write_all(body).expect("health body");
                        }
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    (endpoint, stop)
}

async fn start_fixture(stream_seconds: u64) -> Fixture {
    let (upstream, stop) = start_fake_upstream(stream_seconds);
    let port = free_port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let config_dir = std::env::temp_dir().join(format!(
        "agent-relay-ttl-test-{}-{port}",
        std::process::id()
    ));
    fs::create_dir_all(&config_dir).expect("create fixture directory");
    let config_path = config_dir.join("llama-swap.yaml");
    fs::write(
        &config_path,
        format!(
            "healthCheckTimeout: 10\nglobalTTL: 0\nunloadTimeout: 1\nmodels:\n  ttl-probe:\n    cmd: \"{}\"\n    proxy: {}\n    checkEndpoint: /health\n    ttl: 5\n    unloadTimeout: 1\n",
            sleeper_command().replace('"', "\\\""),
            upstream
        ),
    )
    .expect("write fixture config");
    let child = Command::new(sidecar_path())
        .args([
            "-config",
            config_path.to_str().expect("config path"),
            "-listen",
            &format!("127.0.0.1:{port}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start llama-swap sidecar");
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if client
            .get(format!("{endpoint}/v1/models"))
            .send()
            .await
            .is_ok()
        {
            break;
        }
        assert!(Instant::now() < deadline, "llama-swap did not start");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Fixture {
        child,
        endpoint,
        stop,
        config_dir,
    }
}

async fn running(endpoint: &str) -> bool {
    reqwest::Client::new()
        .get(format!("{endpoint}/running"))
        .send()
        .await
        .expect("read running state")
        .json::<serde_json::Value>()
        .await
        .expect("parse running state")["running"]
        .as_array()
        .is_some_and(|models| {
            models.iter().any(|model| {
                model.get("model").and_then(serde_json::Value::as_str) == Some("ttl-probe")
                    && matches!(
                        model.get("state").and_then(serde_json::Value::as_str),
                        Some("ready" | "starting")
                    )
            })
        })
}

async fn wait_until_unloaded(endpoint: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while running(endpoint).await {
        assert!(
            Instant::now() < deadline,
            "model did not expire after becoming idle"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn stream(endpoint: String) {
    reqwest::Client::new()
        .post(format!("{endpoint}/v1/chat/completions"))
        .json(&serde_json::json!({"model": "ttl-probe", "stream": true}))
        .send()
        .await
        .expect("start streaming request")
        .bytes()
        .await
        .expect("finish streaming request");
}

#[tokio::test]
async fn ttl_starts_after_a_long_stream_completes() {
    let fixture = start_fixture(7).await;
    let request = tokio::spawn(stream(fixture.endpoint.clone()));
    tokio::time::sleep(Duration::from_millis(5_500)).await;
    assert!(
        running(&fixture.endpoint).await,
        "TTL unloaded an in-flight stream"
    );
    request.await.expect("stream task");
    assert!(
        running(&fixture.endpoint).await,
        "model unloaded at stream completion"
    );
    wait_until_unloaded(&fixture.endpoint).await;
}

#[tokio::test]
async fn request_started_just_before_ttl_is_not_cut_off() {
    let fixture = start_fixture(3).await;
    reqwest::Client::new()
        .get(format!("{}/upstream/ttl-probe/v1/models", fixture.endpoint))
        .send()
        .await
        .expect("preload model")
        .error_for_status()
        .expect("preload status");
    tokio::time::sleep(Duration::from_secs(4)).await;
    let request = tokio::spawn(stream(fixture.endpoint.clone()));
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(
        running(&fixture.endpoint).await,
        "TTL cut off a request at load + 5s"
    );
    request.await.expect("stream task");
    wait_until_unloaded(&fixture.endpoint).await;
}
