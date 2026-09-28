// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::num::NonZeroU32;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

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
        Command::new(env!("CARGO_BIN_EXE_devknx"))
            .args(args)
            .env("HOME", self.directory.path())
            .env("XDG_DATA_HOME", self.directory.path())
            .env("LOCALAPPDATA", self.directory.path())
            .output()
            .expect("run isolated devknx CLI")
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

#[tokio::test(flavor = "multi_thread")]
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

#[tokio::test(flavor = "multi_thread")]
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
    assert_eq!(cli.json(&["rest", "--status"])["status"]["enabled"], false);
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

#[tokio::test(flavor = "multi_thread")]
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

#[tokio::test(flavor = "multi_thread")]
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
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn_connect();
    let second = spawn_connect();
    for child in [first, second] {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
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
