// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! A daemon of another build is reported, and `daemon --restart` replaces it.
//!
//! The daemon's build is faked with `DEVKNX_TEST_COMMIT`, which debug builds
//! read in place of the commit they were built from.

use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use knx_rs_ip::DeviceServer;
use serde_json::Value;

/// The commit the old daemon claims to be built from.
const OTHER_COMMIT: &str = "aaaaaaa";

struct Sandbox {
    directory: tempfile::TempDir,
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Sandbox {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().expect("isolated test data directory"),
        }
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devknx"));
        command
            .args(args)
            .env("HOME", self.directory.path())
            .env("XDG_DATA_HOME", self.directory.path())
            .env("LOCALAPPDATA", self.directory.path())
            .env_remove("DEVKNX_TEST_COMMIT")
            .env_remove("DEVKNX_NO_VERSION_WARNING")
            .stdin(Stdio::null());
        for (name, value) in env {
            command.env(name, value);
        }
        command
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let output = self.run_unchecked(args, env);
        assert!(
            output.status.success(),
            "devknx {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// Run a command and give up on it after a minute. Its output goes to
    /// files, not pipes: a daemon it starts may outlive it and would keep a
    /// pipe open, so reading to the end would wait for the daemon, not the
    /// command.
    fn run_unchecked(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let out = tempfile::tempfile().expect("a file for stdout");
        let err = tempfile::tempfile().expect("a file for stderr");
        let mut child = self
            .command(args, env)
            .stdout(out.try_clone().unwrap())
            .stderr(err.try_clone().unwrap())
            .spawn()
            .expect("run isolated devknx CLI");
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "devknx {} did not finish in a minute; stderr so far: {}",
                    args.join(" "),
                    read_all(err)
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        Output {
            status,
            stdout: read_all(out).into_bytes(),
            stderr: read_all(err).into_bytes(),
        }
    }

    /// A foreground daemon claiming another build, ready to answer.
    fn start_other_build(&self) -> ChildGuard {
        let child = self
            .command(&["daemon"], &[("DEVKNX_TEST_COMMIT", OTHER_COMMIT)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the other build's daemon");
        let mut child = ChildGuard(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = json(&self.run(&["daemon", "--status"], &[]));
            if status["running"] == true {
                return child;
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "the daemon exited early"
            );
            assert!(Instant::now() < deadline, "the daemon did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self
            .command(&["daemon", "--stop"], &[])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn read_all(mut file: std::fs::File) -> String {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.read_to_string(&mut text).unwrap();
    text
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one JSON document ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A command that uses the daemon warns once on stderr when the daemon is
/// another build. Stdout stays one JSON document. The same build, and the
/// opt-out, stay silent.
#[test]
fn a_daemon_of_another_build_is_reported_on_stderr_only() {
    let sandbox = Sandbox::new();
    let _daemon = sandbox.start_other_build();

    let warned = sandbox.run(&["sessions"], &[]);
    let said = stderr(&warned);
    assert!(said.contains(OTHER_COMMIT), "{said}");
    assert!(said.contains("devknx daemon --restart"), "{said}");
    assert_eq!(said.matches("warning:").count(), 1, "{said}");
    assert!(json(&warned)["sessions"].is_array());

    let status = sandbox.run(&["daemon", "--status"], &[]);
    assert_eq!(json(&status)["build"]["commit"], OTHER_COMMIT);
    assert!(stderr(&status).contains("devknx daemon --restart"));

    // Counter-probes: the same build, and the opt-out.
    let same = sandbox.run(&["sessions"], &[("DEVKNX_TEST_COMMIT", OTHER_COMMIT)]);
    assert!(stderr(&same).is_empty(), "{}", stderr(&same));
    let quiet = sandbox.run(&["sessions"], &[("DEVKNX_NO_VERSION_WARNING", "1")]);
    assert!(stderr(&quiet).is_empty(), "{}", stderr(&quiet));
}

/// Without a daemon there is nothing to compare, and nothing is reported.
#[test]
fn no_daemon_means_no_warning() {
    let sandbox = Sandbox::new();
    let status = sandbox.run(&["daemon", "--status"], &[]);
    assert_eq!(json(&status)["running"], false);
    assert!(stderr(&status).is_empty(), "{}", stderr(&status));
}

/// `--restart` stops the other build, starts this one and connects its KNX
/// sessions again with their database and retention limit. A REST listener
/// is reported, not restored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_brings_up_the_callers_build_and_reconnects_sessions() {
    let sandbox = Sandbox::new();
    let mut old = sandbox.start_other_build();
    let gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let endpoint = format!("tunnel://{}", gateway.local_addr());
    let connected = json(&sandbox.run(&["connect", &endpoint, "--max-events", "500"], &[]));
    let database = connected["session"]["database"].clone();
    sandbox.run(&["rest", "--enable", "--bind", "127.0.0.1:0"], &[]);

    let restarted = sandbox.run(&["daemon", "--restart"], &[]);
    let report = json(&restarted);
    assert_eq!(report["was_running"], true, "{report}");
    assert_ne!(report["identity"]["commit"], OTHER_COMMIT, "{report}");
    assert_eq!(report["rest_was_enabled"], true, "{report}");
    assert!(stderr(&restarted).contains("rest --enable"));
    let reconnected = report["reconnected"].as_array().unwrap();
    assert_eq!(reconnected.len(), 1, "{report}");
    assert_eq!(reconnected[0]["database"], database);
    assert_eq!(reconnected[0]["max_events"], 500);
    assert!(report["failed"].as_array().unwrap().is_empty(), "{report}");

    let deadline = Instant::now() + Duration::from_secs(5);
    while old.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the old daemon still runs");
        std::thread::sleep(Duration::from_millis(25));
    }

    // The daemon is now this build: no warning, and the session is there.
    let sessions = sandbox.run(&["sessions"], &[]);
    assert!(stderr(&sessions).is_empty(), "{}", stderr(&sessions));
    assert_eq!(json(&sessions)["sessions"][0]["database"], database);
    assert_eq!(
        json(&sandbox.run(&["rest", "--status"], &[]))["status"]["enabled"],
        false
    );
}
