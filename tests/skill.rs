// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! `devknx skill` against a temporary home, so no test reaches the real
//! skills folders of the AI tools on the machine.

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// `devknx skill ARGS`, seeing only the AI tools under `home`.
fn skill(home: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_devknx"));
    command
        .arg("skill")
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_DATA_HOME", home)
        .env("LOCALAPPDATA", home)
        .stdin(Stdio::null());
    for var in [
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "COPILOT_HOME",
        "XDG_CONFIG_HOME",
    ] {
        command.env_remove(var);
    }
    command.output().expect("run devknx skill")
}

fn status(home: &Path) -> String {
    let output = skill(home, &[]);
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn tool(home: &Path, folder: &str) {
    std::fs::create_dir_all(home.join(folder)).expect("tool folder");
}

/// The skill goes only where the user agreed, and comes out again.
#[test]
fn the_skill_is_installed_only_with_consent() {
    let directory = tempfile::tempdir().expect("home");
    let home = directory.path();
    tool(home, ".claude");
    let installed = home.join(".claude/skills/devknx");

    let shown = status(home);
    assert!(shown.contains("Claude Code"), "{shown}");
    assert!(shown.contains("waiting for your consent"), "{shown}");

    let refused = skill(home, &["--install"]);
    assert!(
        !refused.status.success(),
        "installed without a terminal or --yes"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--yes"),
        "{refused:?}"
    );
    assert!(!installed.exists(), "installed without consent");

    let done = skill(home, &["--install", "--yes"]);
    assert!(done.status.success(), "{done:?}");
    let skill_md = std::fs::read_to_string(installed.join("SKILL.md")).expect("SKILL.md");
    assert!(skill_md.contains("name: \"devknx\""), "{skill_md}");
    assert!(installed.join("references/cli.md").exists());
    assert!(installed.join("references/mcp.md").exists());
    assert!(installed.join("references/http.md").exists());
    assert!(status(home).contains("installed and up to date"));

    let removed = skill(home, &["--remove"]);
    assert!(removed.status.success(), "{removed:?}");
    assert!(!installed.exists(), "the skill outlived --remove");
}

/// A declined place is not offered again; a tool found later is asked about
/// on its own; an explicit install still goes everywhere.
#[test]
fn a_declined_place_is_not_offered_again() {
    let directory = tempfile::tempdir().expect("home");
    let home = directory.path();
    tool(home, ".claude");

    assert!(skill(home, &["--decline"]).status.success());
    let shown = status(home);
    assert!(shown.contains("declined"), "{shown}");
    assert!(!shown.contains("waiting for your consent"), "{shown}");

    tool(home, ".codex");
    let shown = status(home);
    assert!(shown.contains("declined"), "{shown}");
    assert!(shown.contains("waiting for your consent"), "{shown}");

    assert!(skill(home, &["--install", "--yes"]).status.success());
    assert!(home.join(".claude/skills/devknx/SKILL.md").exists());
    assert!(home.join(".agents/skills/devknx/SKILL.md").exists());
}

/// A copy an older devknx installed is brought up to date, even when no
/// place is new.
#[test]
fn an_older_copy_is_updated_by_install() {
    use agent_skill_installer::{AgentSkill, Environment, Program, Skill};

    let directory = tempfile::tempdir().expect("home");
    let home = directory.path();
    tool(home, ".claude");
    let older = AgentSkill::new(
        Skill::new("devknx", "An older devknx.", "old\n").expect("skill"),
        Program::new("devknx", "0.0.1").expect("program"),
        home.join("older-consent"),
    )
    .expect("home")
    .with_environment(Environment::new(home));
    older
        .offer()
        .expect("offer")
        .expect("a place")
        .accept()
        .expect("install");

    let updated = skill(home, &["--install", "--yes"]);
    assert!(updated.status.success(), "{updated:?}");
    assert!(
        String::from_utf8_lossy(&updated.stdout).contains("updated from 0.0.1"),
        "{updated:?}"
    );
    assert!(status(home).contains("installed and up to date"));
}

/// Nothing to offer on a machine without AI tools.
#[test]
fn without_an_ai_tool_nothing_is_offered() {
    let directory = tempfile::tempdir().expect("home");
    assert!(status(directory.path()).contains("No AI tool"));
}
