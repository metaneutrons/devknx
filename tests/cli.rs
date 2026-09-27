// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::process::Command;

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
