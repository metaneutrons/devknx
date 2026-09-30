// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::io::{Read as _, Seek as _};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use devknx::ipc::{IpcMessage, WireState};
use devknx::storage::CaptureStore;
use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
use knx_rs_core::cemi::CemiFrame;
use knx_rs_core::message::MessageCode;
use knx_rs_core::types::Priority;
use knx_rs_ip::DeviceServer;
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

struct IsolatedCli {
    directory: tempfile::TempDir,
}

struct ChildGuard(Child);

fn wait_for_cli(mut child: Child, command: &str) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().expect("query CLI process") {
            return status;
        }
        if Instant::now() >= deadline {
            eprintln!("timed out waiting for devknx {command}");
            let _ = child.kill();
            return child.wait().expect("reap timed-out CLI process");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl IsolatedCli {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().expect("isolated test data directory"),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut stdout = tempfile::tempfile().expect("temporary CLI stdout");
        let mut stderr = tempfile::tempfile().expect("temporary CLI stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_devknx"))
            .args(args)
            .env("HOME", self.directory.path())
            .env("XDG_DATA_HOME", self.directory.path())
            .env("LOCALAPPDATA", self.directory.path())
            .stdout(Stdio::from(stdout.try_clone().expect("clone CLI stdout")))
            .stderr(Stdio::from(stderr.try_clone().expect("clone CLI stderr")))
            .spawn()
            .expect("run isolated devknx CLI");
        let status = wait_for_cli(child, &args.join(" "));
        stdout.rewind().expect("rewind CLI stdout");
        stderr.rewind().expect("rewind CLI stderr");
        let mut stdout_bytes = Vec::new();
        let mut stderr_bytes = Vec::new();
        stdout
            .read_to_end(&mut stdout_bytes)
            .expect("read CLI stdout");
        stderr
            .read_to_end(&mut stderr_bytes)
            .expect("read CLI stderr");
        Output {
            status,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        }
    }

    fn application_data_dir(&self) -> std::path::PathBuf {
        #[cfg(target_os = "macos")]
        let base = self.directory.path().join("Library/Application Support");
        #[cfg(not(target_os = "macos"))]
        let base = self.directory.path().to_path_buf();
        base.join("devknx")
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("JSON control reply")
    }
}

impl Drop for IsolatedCli {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "--stop"]);
    }
}

async fn wait_connected(cli: &IsolatedCli, endpoint: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let output = cli.run(&["status", "--endpoint", endpoint]);
            if output.status.success()
                && matches!(
                    serde_json::from_slice::<IpcMessage>(&output.stdout),
                    Ok(IpcMessage::State {
                        value: WireState::Connected { .. },
                        ..
                    })
                )
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("session connected to loopback gateway");
}

async fn http_json(address: SocketAddr, method: &str, path: &str) -> (u16, Value) {
    http_json_body(address, method, path, None).await
}

async fn http_json_body(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to REST listener");
    let body = body.map_or_else(String::new, |value| value.to_string());
    stream
        .write_all(
            format!(
                "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("send REST request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("read REST reply");
    let separator = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .expect("REST reply headers");
    let status = String::from_utf8_lossy(&response[..separator])
        .split_whitespace()
        .nth(1)
        .expect("REST status code")
        .parse()
        .expect("numeric REST status");
    let body = serde_json::from_slice(&response[separator + 4..]).expect("JSON REST reply");
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rest_without_default_endpoint_controls_isolated_sessions() {
    let cli = IsolatedCli::new();
    let first_gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let second_gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let first = format!("tunnel://{}", first_gateway.local_addr());
    let second = format!("tunnel://{}", second_gateway.local_addr());
    let enabled = cli.json(&["rest", "--enable", "--bind", "127.0.0.1:0"]);
    assert!(enabled["status"]["endpoint"].is_null());
    let address: SocketAddr = enabled["status"]["bind"].as_str().unwrap().parse().unwrap();
    let (code, empty) = http_json(address, "GET", "/v1/sessions").await;
    assert_eq!(code, 200);
    assert!(empty["sessions"].as_array().unwrap().is_empty());
    let (code, missing) = http_json(address, "GET", "/v1/captures").await;
    assert_eq!(code, 400);
    assert_eq!(missing["error"], "explicit endpoint is required");
    let (code, missing) = http_json_body(
        address,
        "POST",
        "/v1/operations/read",
        Some(serde_json::json!({"address":"1/2/3"})),
    )
    .await;
    assert_eq!(code, 400);
    assert_eq!(missing["error"], "explicit endpoint is required");
    let (code, started_first) = http_json_body(
        address,
        "POST",
        "/v1/sessions",
        Some(serde_json::json!({"endpoint":first})),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(started_first["session"]["endpoint"], first);
    let (code, started_second) = http_json_body(
        address,
        "POST",
        "/v1/sessions",
        Some(serde_json::json!({"endpoint":second})),
    )
    .await;
    assert_eq!(code, 200);
    assert_ne!(
        started_first["session"]["database"],
        started_second["session"]["database"]
    );
    wait_connected(&cli, &first).await;
    wait_connected(&cli, &second).await;
    first_gateway
        .send_frame(CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
            Priority::Low,
            &[0x00, 0x40, 0x2a],
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (code, page) =
                http_json(address, "GET", &format!("/v1/captures?endpoint={first}")).await;
            assert_eq!(code, 200);
            if !page["items"].as_array().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("first session capture visible through REST");
    let (code, listed) = http_json(address, "GET", "/v1/sessions").await;
    assert_eq!(code, 200);
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 2);
    let (code, first_page) =
        http_json(address, "GET", &format!("/v1/captures?endpoint={first}")).await;
    assert_eq!(code, 200);
    assert_eq!(first_page["items"].as_array().unwrap().len(), 1);
    assert_eq!(first_page["items"][0]["endpoint"], first);
    let (code, second_page) =
        http_json(address, "GET", &format!("/v1/captures?endpoint={second}")).await;
    assert_eq!(code, 200);
    assert!(second_page["items"].as_array().unwrap().is_empty());
    let (code, stopped) =
        http_json(address, "DELETE", &format!("/v1/sessions?endpoint={first}")).await;
    assert_eq!(code, 200);
    assert_eq!(stopped["disconnected"], true);
    assert_eq!(cli.json(&["rest", "--status"])["status"]["enabled"], true);
    let (code, listed) = http_json(address, "GET", "/v1/sessions").await;
    assert_eq!(code, 200);
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["endpoint"], second);
    cli.json(&["rest", "--disable"]);
    first_gateway.stop().await;
    second_gateway.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreground_daemon_starts_without_a_knx_session() {
    let cli = IsolatedCli::new();
    let child = Command::new(env!("CARGO_BIN_EXE_devknx"))
        .arg("daemon")
        .env("HOME", cli.directory.path())
        .env("XDG_DATA_HOME", cli.directory.path())
        .env("LOCALAPPDATA", cli.directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if cli.json(&["daemon", "--status"])["running"]
                .as_bool()
                .unwrap_or(false)
            {
                break;
            }
            assert!(child.0.try_wait().unwrap().is_none(), "daemon exited early");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("foreground daemon became ready");
    assert!(
        cli.json(&["sessions"])["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!cli.application_data_dir().join("captures").exists());
    assert_eq!(cli.json(&["rest", "--status"])["status"]["enabled"], false);
    cli.json(&["daemon", "--stop"]);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if child.0.try_wait().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("foreground daemon stopped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rest_starts_offline_and_controls_its_selected_knx_session() {
    let cli = IsolatedCli::new();
    let gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let endpoint = format!("tunnel://{}", gateway.local_addr());
    let database = cli.directory.path().join("selected.sqlite");
    let database_text = database.to_str().unwrap();
    let enabled = cli.json(&[
        "rest",
        "--enable",
        "--endpoint",
        &endpoint,
        "--database",
        database_text,
        "--bind",
        "127.0.0.1:0",
    ]);
    let address: SocketAddr = enabled["status"]["bind"].as_str().unwrap().parse().unwrap();
    assert_eq!(enabled["status"]["endpoint"], endpoint);
    assert!(
        !database.exists(),
        "REST started a capture without a Connect request"
    );
    assert!(
        cli.json(&["sessions"])["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let (code, health) = http_json(address, "GET", "/v1/health").await;
    assert_eq!(code, 200);
    assert_eq!(health["api"], "ready");
    assert!(health["capture_owner"].is_null());
    let (code, captures) = http_json(address, "GET", "/v1/captures?after=0&limit=10").await;
    assert_eq!(code, 200);
    assert!(captures["items"].as_array().unwrap().is_empty());
    let (code, before) = http_json(address, "GET", "/v1/connection").await;
    assert_eq!(code, 200);
    assert_eq!(before["endpoint"], endpoint);
    assert_eq!(before["session_active"], false);

    let (code, started) = http_json(address, "POST", "/v1/connection").await;
    assert_eq!(code, 200);
    assert_eq!(
        started["session"]["database"],
        database.canonicalize().unwrap().to_str().unwrap()
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (code, state) = http_json(address, "GET", "/v1/connection").await;
            assert_eq!(code, 200);
            if state["capture_owner"]["value"]["state"] == "connected" {
                assert_eq!(state["session_active"], true);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("REST connected its KNX endpoint");

    let (code, stopped) = http_json(address, "DELETE", "/v1/connection").await;
    assert_eq!(code, 200);
    assert_eq!(stopped["disconnected"], true);
    assert_eq!(cli.json(&["rest", "--status"])["status"]["enabled"], true);
    let (code, after) = http_json(address, "GET", "/v1/connection").await;
    assert_eq!(code, 200);
    assert_eq!(after["session_active"], false);
    assert!(after["capture_owner"].is_null());
    cli.json(&["rest", "--disable"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[expect(
    clippy::too_many_lines,
    reason = "one end-to-end multi-session lifecycle scenario"
)]
async fn daemon_manages_two_isolated_sessions_and_rest_lifecycle() {
    let cli = IsolatedCli::new();
    assert_eq!(cli.json(&["daemon", "--status"])["running"], false);
    assert!(!cli.application_data_dir().exists());

    let first_gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let second_gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let first = format!("tunnel://{}", first_gateway.local_addr());
    let second = format!("tunnel://{}", second_gateway.local_addr());

    let connected_first = cli.json(&["connect", &first]);
    let first_database = connected_first["session"]["database"]
        .as_str()
        .expect("first database")
        .to_owned();
    wait_connected(&cli, &first).await;

    let conflicting = cli.run(&["connect", &second, "--database", &first_database]);
    assert!(
        !conflicting.status.success(),
        "second endpoint reused first database"
    );

    let connected_second = cli.json(&["connect", &second]);
    let second_database = connected_second["session"]["database"]
        .as_str()
        .expect("second database")
        .to_owned();
    assert_ne!(first_database, second_database);
    wait_connected(&cli, &second).await;
    assert_eq!(
        cli.json(&["sessions"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    first_gateway
        .send_frame(CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
            Priority::Low,
            &[0x00, 0x40, 0x2a],
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let first_store = CaptureStore::open_existing(Path::new(&first_database)).unwrap();
            if !first_store
                .read_after(0, NonZeroU32::new(10).unwrap())
                .unwrap()
                .is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("first session committed its frame");
    assert!(
        CaptureStore::open_existing(Path::new(&second_database))
            .unwrap()
            .read_after(0, NonZeroU32::new(10).unwrap())
            .unwrap()
            .is_empty(),
        "second session received the first session's capture"
    );

    let unsafe_rest = cli.run(&[
        "rest",
        "--enable",
        "--endpoint",
        &second,
        "--bind",
        "0.0.0.0:0",
    ]);
    assert!(!unsafe_rest.status.success());
    assert_eq!(cli.json(&["rest", "--status"])["status"]["enabled"], false);

    let rest = cli.json(&[
        "rest",
        "--enable",
        "--endpoint",
        &second,
        "--bind",
        "127.0.0.1:0",
    ]);
    let bind = rest["status"]["bind"].as_str().unwrap();
    assert_ne!(bind, "127.0.0.1:0");
    assert_eq!(rest["status"]["endpoint"], second);
    assert!(rest["status"].get("token").is_none());
    let mut stream = tokio::net::TcpStream::connect(bind).await.unwrap();
    stream
        .write_all(
            format!("GET /v1/health HTTP/1.1\r\nHost: {bind}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"));

    assert_eq!(cli.json(&["rest", "--disable"])["status"]["enabled"], false);
    assert_eq!(cli.json(&["rest", "--status"])["status"]["enabled"], false);
    assert_eq!(
        cli.json(&["sessions"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "disabling REST stopped a KNX session"
    );
    cli.json(&[
        "rest",
        "--enable",
        "--endpoint",
        &second,
        "--bind",
        "127.0.0.1:0",
    ]);

    cli.json(&["disconnect", &first]);
    assert_eq!(cli.json(&["daemon", "--status"])["running"], true);
    assert_eq!(
        cli.json(&["sessions"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(!cli.run(&["status", "--endpoint", &first]).status.success());
    assert!(cli.run(&["status", "--endpoint", &second]).status.success());
    drop(
        CaptureStore::open(Path::new(&first_database), NonZeroU32::new(100).unwrap())
            .expect("first writer lease released"),
    );

    cli.json(&["disconnect", &second]);
    assert_eq!(cli.json(&["rest", "--status"])["status"]["enabled"], true);
    assert_eq!(
        cli.json(&["sessions"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let historical_reuse = cli.run(&["connect", &second, "--database", &first_database]);
    assert!(
        !historical_reuse.status.success(),
        "persisted binding was ignored"
    );
    assert_eq!(cli.json(&["daemon", "--status"])["running"], true);

    cli.json(&["daemon", "--stop"]);
    assert_eq!(cli.json(&["daemon", "--status"])["running"], false);
    first_gateway.stop().await;
    second_gateway.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn endpoint_selected_read_auto_starts_daemon_without_a_manual_connect() {
    let cli = IsolatedCli::new();
    let gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let endpoint = format!("tunnel://{}", gateway.local_addr());
    assert_eq!(cli.json(&["daemon", "--status"])["running"], false);
    let read = cli.run(&[
        "read",
        "--endpoint",
        &endpoint,
        "--timeout-ms",
        "20",
        "1/2/3",
    ]);
    assert!(
        read.status.success(),
        "{}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert_eq!(cli.json(&["daemon", "--status"])["running"], true);
    assert_eq!(
        cli.json(&["sessions"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    gateway.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_connect_commands_share_one_daemon_and_session() {
    let cli = IsolatedCli::new();
    let gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let endpoint = format!("tunnel://{}", gateway.local_addr());
    let spawn_connect = || {
        Command::new(env!("CARGO_BIN_EXE_devknx"))
            .args(["connect", &endpoint])
            .env("HOME", cli.directory.path())
            .env("XDG_DATA_HOME", cli.directory.path())
            .env("LOCALAPPDATA", cli.directory.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    };
    let first = spawn_connect();
    let second = spawn_connect();
    for child in [first, second] {
        let status = wait_for_cli(child, "connect (concurrent)");
        assert!(
            status.success(),
            "concurrent connect process failed: {status}"
        );
    }
    assert_eq!(
        cli.json(&["sessions"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    wait_connected(&cli, &endpoint).await;
    gateway.stop().await;
}
