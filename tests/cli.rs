// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::num::NonZeroU32;
use std::process::Command;

use devknx::capture::{CaptureEndpoint, CaptureEvent};
use devknx::storage::CaptureStore;
use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
use knx_rs_core::cemi::CemiFrame;
use knx_rs_core::message::MessageCode;
use knx_rs_core::types::Priority;

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
