// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::io::Read as _;
use std::num::NonZeroU32;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use devknx::capture::{CaptureEndpoint, CaptureEvent};
use devknx::storage::CaptureStore;
use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
use knx_rs_core::cemi::CemiFrame;
use knx_rs_core::message::MessageCode;
use knx_rs_core::types::Priority;
use knx_rs_ip::DeviceServer;

fn devknx(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_devknx"))
        .args(args)
        .output()
        .expect("run devknx")
}

#[test]
fn help_describes_available_commands() {
    let output = devknx(&["--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(stdout.contains("discover"));
    assert!(stdout.contains("monitor"));
    assert!(stdout.contains("serve"));
    assert!(stdout.contains("history"));
    assert!(stdout.contains("export"));
    #[cfg(feature = "gui")]
    assert!(stdout.contains("gui"));
    #[cfg(not(feature = "gui"))]
    assert!(!stdout.contains("gui"));
}

#[test]
fn monitor_rejects_invalid_endpoint_without_connecting() {
    let output = devknx(&["monitor", "http://192.0.2.1:3671"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("unsupported scheme"));
}

#[test]
fn serve_rejects_a_second_capture_writer() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("captures.sqlite");
    let writer = CaptureStore::open(&path, NonZeroU32::new(10).expect("nonzero"))
        .expect("create capture store");
    let output = devknx(&[
        "serve",
        "tunnel://192.0.2.1:3671",
        "--database",
        path.to_str().expect("UTF-8 test path"),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("WriterBusy"), "{stderr}");
    drop(writer);
    assert!(CaptureStore::open(&path, NonZeroU32::new(10).expect("nonzero")).is_ok());
}

#[tokio::test]
async fn serve_recovers_committed_history_after_process_termination() {
    struct CaptureChild(Child);

    impl Drop for CaptureChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("service.sqlite");
    let database = path.to_str().expect("UTF-8 test path");

    for value in [41_u8, 42_u8] {
        let server = DeviceServer::start_at("127.0.0.1:0".parse().expect("loopback address"))
            .await
            .expect("start KNX tunnel server");
        let endpoint = format!("tunnel://{}", server.local_addr());
        let child = Command::new(env!("CARGO_BIN_EXE_devknx"))
            .args(["serve", &endpoint, "--database", database])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start capture process");
        let mut child = CaptureChild(child);
        let found = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(status) = child.0.try_wait().expect("query capture process") {
                    let mut stderr = String::new();
                    if let Some(mut stream) = child.0.stderr.take() {
                        stream
                            .read_to_string(&mut stderr)
                            .expect("read error output");
                    }
                    panic!("capture process exited with {status}: {stderr}");
                }
                let frame = CemiFrame::new_l_data(
                    MessageCode::LDataInd,
                    IndividualAddress::from_raw(0x1101),
                    DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
                    Priority::Low,
                    &[0x00, 0x80, value],
                );
                server.send_frame(frame).await.expect("send loopback frame");
                if let Ok(history) = CaptureStore::open_existing(&path)
                    && history
                        .read_after(0, NonZeroU32::new(1_000).expect("nonzero"))
                        .expect("read committed history")
                        .iter()
                        .any(|capture| capture.event.frame().payload() == [0x00, 0x80, value])
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(found.is_ok(), "capture process timed out on value {value}");
        child.0.kill().expect("terminate capture process");
        child.0.wait().expect("reap capture process");
        server.stop().await;
    }

    let history = CaptureStore::open_existing(&path).expect("reopen after second termination");
    let values: Vec<u8> = history
        .read_after(0, NonZeroU32::new(1_000).expect("nonzero"))
        .expect("read committed history")
        .into_iter()
        .map(|capture| capture.event.frame().payload()[2])
        .collect();
    assert!(values.contains(&41), "first process capture was lost");
    assert!(values.contains(&42), "second process capture was lost");
}

#[test]
fn history_and_export_read_the_same_committed_frame() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("captures.sqlite");
    let mut store = CaptureStore::open(&path, NonZeroU32::new(10).expect("nonzero"))
        .expect("create capture store");
    let frame = CemiFrame::new_l_data(
        MessageCode::LDataInd,
        IndividualAddress::from_raw(0x1101),
        DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
        Priority::Low,
        &[0x00, 0x80, 0x01],
    );
    let event = CaptureEvent::received(
        CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().expect("socket address")),
        frame,
    );
    assert_eq!(store.insert(&event).expect("insert frame"), 1);
    drop(store);

    let path = path.to_str().expect("UTF-8 test path");
    let history = devknx(&["history", "--database", path, "--after", "0"]);
    assert!(history.status.success());
    let history = String::from_utf8(history.stdout).expect("UTF-8 history");
    assert!(history.contains("id=1 timestamp_ms="));
    assert!(history.contains("cemi=2900bce01101080102008001"));

    let export = devknx(&["export", "--database", path]);
    assert!(export.status.success());
    let export = String::from_utf8(export.stdout).expect("UTF-8 CSV");
    assert_eq!(export.lines().count(), 2);
    assert!(export.contains(",GroupValueWrite,0x29,2900bce01101080102008001"));

    let empty = devknx(&["history", "--database", path, "--after", "1"]);
    assert!(empty.status.success());
    assert!(empty.stdout.is_empty());
}

#[test]
fn history_does_not_create_a_missing_database() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("missing.sqlite");
    let output = devknx(&[
        "history",
        "--database",
        path.to_str().expect("UTF-8 test path"),
    ]);
    assert!(!output.status.success());
    assert!(!path.exists());
}

#[test]
fn version_matches_package() {
    let output = devknx(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("UTF-8 version")
            .trim(),
        concat!("devknx ", env!("CARGO_PKG_VERSION"))
    );
}
