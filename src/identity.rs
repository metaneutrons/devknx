// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Which build a process is, so a client can tell when the daemon it talks to
//! is a different one .
//!
//! Version numbers alone do not tell: a development build and the installed
//! release can both say 0.2.7 and run different code. The commit and whether
//! the tree had changes do.

use serde::{Deserialize, Serialize};

/// The environment variable that silences the warning, for someone running a
/// different build as the daemon on purpose while developing.
pub const OPT_OUT: &str = "DEVKNX_NO_VERSION_WARNING";

/// What a build says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildIdentity {
    /// The crate version.
    pub version: String,
    /// The commit, abbreviated; `None` when the build had no git.
    pub commit: Option<String>,
    /// Whether the tree had uncommitted changes; `None` when unknown.
    pub dirty: Option<bool>,
    /// When it was built; tells two builds of a modified tree apart.
    pub stamp: String,
    /// Where the running executable is, when known.
    pub executable: Option<String>,
}

impl BuildIdentity {
    /// This build, as compiled.
    #[must_use]
    pub fn current() -> Self {
        let commit = env!("DEVKNX_COMMIT");
        let dirty = match env!("DEVKNX_DIRTY") {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        };
        let identity = Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: (!commit.is_empty()).then(|| commit.to_string()),
            dirty,
            stamp: env!("DEVKNX_BUILD_STAMP").to_string(),
            executable: std::env::current_exe()
                .ok()
                .map(|path| path.display().to_string()),
        };
        #[cfg(debug_assertions)]
        let identity = test_override(identity);
        identity
    }

    /// The build in a few words: `0.2.7 (a1b2c3d, modified)`.
    #[must_use]
    pub fn short(&self) -> String {
        match (&self.commit, self.dirty) {
            (Some(commit), Some(true)) => format!("{} ({commit}, modified)", self.version),
            (Some(commit), _) => format!("{} ({commit})", self.version),
            (None, _) => format!("{} (commit unknown)", self.version),
        }
    }
}

/// A test may make a daemon claim another commit, to pair two identities
/// through the real binary. Only in debug builds; releases are built with
/// `--release`.
#[cfg(debug_assertions)]
fn test_override(mut identity: BuildIdentity) -> BuildIdentity {
    if let Ok(commit) = std::env::var("DEVKNX_TEST_COMMIT") {
        identity.commit = (!commit.is_empty()).then_some(commit);
        identity.dirty = Some(false);
    }
    identity
}

/// How a daemon's build differs from the client's, if it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    /// Another version or another commit.
    Different,
    /// The same commit, but one side was built from a modified tree.
    Modified,
    /// The same version, and a commit unknown on one side, or on both with
    /// builds of different moments.
    CannotTell,
}

/// Compare the daemon's build with the client's.
///
/// The same commit from clean trees is the same code, wherever each is
/// installed, so the paths do not count.
#[must_use]
pub fn compare(client: &BuildIdentity, daemon: &BuildIdentity) -> Option<Mismatch> {
    match (&client.commit, &daemon.commit) {
        (Some(ours), Some(theirs)) if ours != theirs => Some(Mismatch::Different),
        (Some(_), Some(_)) if client.version != daemon.version => Some(Mismatch::Different),
        (Some(_), Some(_)) => match (client.dirty, daemon.dirty) {
            (Some(false), Some(false)) => None,
            // Both from a modified tree: the same build only if built at
            // the same moment, which is the same binary.
            (Some(true), Some(true)) if client.stamp == daemon.stamp => None,
            _ => Some(Mismatch::Modified),
        },
        _ if client.version != daemon.version => Some(Mismatch::Different),
        // No git on either side, as in a build from a source archive: the
        // build stamp says whether it is the same binary, which it is for a
        // client and the daemon the same installation started.
        (None, None) if client.stamp == daemon.stamp => None,
        _ => Some(Mismatch::CannotTell),
    }
}

/// The warning for a mismatch, one line, or `None` when the builds agree or
/// the user opted out.
#[must_use]
pub fn warning(client: &BuildIdentity, daemon: &BuildIdentity) -> Option<String> {
    if std::env::var_os(OPT_OUT).is_some_and(|value| !value.is_empty()) {
        return None;
    }
    let how = compare(client, daemon)?;
    let from = daemon
        .executable
        .as_deref()
        .map_or_else(String::new, |path| format!(" from {path}"));
    let lead = match how {
        Mismatch::Different | Mismatch::Modified => "the daemon is",
        Mismatch::CannotTell => "cannot tell whether the daemon is the same build: it is",
    };
    Some(format!(
        "warning: {lead} {}{from}; this is {}. Restart it with: devknx daemon --restart",
        daemon.short(),
        client.short()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(
        version: &str,
        commit: Option<&str>,
        dirty: Option<bool>,
        path: &str,
    ) -> BuildIdentity {
        BuildIdentity {
            version: version.to_string(),
            commit: commit.map(str::to_string),
            dirty,
            stamp: "1".to_string(),
            executable: Some(path.to_string()),
        }
    }

    /// #115: every row of the table, and identical builds as the
    /// counter-probe.
    #[test]
    fn the_comparison_follows_the_table() {
        let release = build(
            "0.2.7",
            Some("9f8e7d6"),
            Some(false),
            "/opt/homebrew/bin/devserial",
        );

        // Identical: nothing.
        assert_eq!(compare(&release, &release.clone()), None);
        // Another version, or another commit.
        assert_eq!(
            compare(
                &release,
                &build("0.2.8", Some("9f8e7d6"), Some(false), "/x")
            ),
            Some(Mismatch::Different)
        );
        assert_eq!(
            compare(
                &release,
                &build("0.2.7", Some("a1b2c3d"), Some(false), "/x")
            ),
            Some(Mismatch::Different)
        );
        // Same commit, either side modified: the development case.
        assert_eq!(
            compare(
                &release,
                &build(
                    "0.2.7",
                    Some("9f8e7d6"),
                    Some(true),
                    "/target/debug/devserial"
                )
            ),
            Some(Mismatch::Modified)
        );
        assert_eq!(
            compare(&build("0.2.7", Some("9f8e7d6"), Some(true), "/x"), &release),
            Some(Mismatch::Modified)
        );
        // Same commit, both modified: the same binary when built at the same
        // moment, another one otherwise.
        let dev = build(
            "0.2.7",
            Some("9f8e7d6"),
            Some(true),
            "/target/debug/devserial",
        );
        assert_eq!(compare(&dev, &dev.clone()), None);
        let rebuilt = BuildIdentity {
            stamp: "2".to_string(),
            ..dev.clone()
        };
        assert_eq!(compare(&rebuilt, &dev), Some(Mismatch::Modified));
        // Same commit, both clean, installed elsewhere: the same code.
        assert_eq!(
            compare(
                &release,
                &build(
                    "0.2.7",
                    Some("9f8e7d6"),
                    Some(false),
                    "/home/u/.cargo/bin/devserial"
                )
            ),
            None
        );
        // A commit unknown, same version: cannot tell.
        assert_eq!(
            compare(&release, &build("0.2.7", None, None, "/x")),
            Some(Mismatch::CannotTell)
        );
        // No commit on either side, the same version and the same build
        // stamp: the same binary, as for a package built from a source
        // archive. The counter-probe is another stamp, which cannot be told.
        let archive = build("0.2.7", None, None, "/usr/bin/devknx");
        assert_eq!(compare(&archive, &archive.clone()), None);
        let rebuilt = BuildIdentity {
            stamp: "2".to_string(),
            ..archive.clone()
        };
        assert_eq!(compare(&rebuilt, &archive), Some(Mismatch::CannotTell));
        // A commit unknown, another version: different.
        assert_eq!(
            compare(&release, &build("0.2.6", None, None, "/x")),
            Some(Mismatch::Different)
        );
    }

    /// The warning names both builds and the daemon's path, and the restart.
    #[test]
    fn the_warning_names_both_builds() {
        let client = build(
            "0.2.7",
            Some("9f8e7d6"),
            Some(false),
            "/opt/homebrew/bin/devserial",
        );
        let daemon = build(
            "0.2.7",
            Some("a1b2c3d"),
            Some(true),
            "/src/target/debug/devserial",
        );
        assert_eq!(
            warning(&client, &daemon).as_deref(),
            Some(
                "warning: the daemon is 0.2.7 (a1b2c3d, modified) from /src/target/debug/devserial; \
                 this is 0.2.7 (9f8e7d6). Restart it with: devknx daemon --restart"
            )
        );
        assert_eq!(warning(&client, &client.clone()), None);
    }
}
