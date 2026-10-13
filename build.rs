// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

fn main() {
    build_identity();

    #[cfg(target_os = "macos")]
    {
        println!("cargo:rerun-if-changed=resources/macos.m");
        cc::Build::new()
            .file("resources/macos.m")
            .compile("devknx_macos");
        println!("cargo:rustc-link-lib=framework=AppKit");
    }
    #[cfg(windows)]
    {
        let mut resource = winres::WindowsResource::new();
        resource.set_icon("resources/devknx.ico");
        resource.compile().expect("embed devknx icon");
    }
}

/// The commit the binary is built from, whether the tree had changes, and
/// when it was built, so a client can tell a daemon from another build.
/// Without git, as from a source archive, the commit stays empty and a
/// comparison says it cannot tell.
fn build_identity() {
    // The tree can change without a commit, so any source change has to run
    // this again; a stale "clean" would hide the development case.
    for path in [
        "src",
        "resources",
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        ".git/HEAD",
        ".git/index",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let commit = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_default();
    let dirty = if commit.is_empty() {
        ""
    } else {
        match git(&["status", "--porcelain", "--untracked-files=no"]) {
            Some(changes) if changes.is_empty() => "0",
            Some(_) => "1",
            None => "",
        }
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    println!("cargo:rustc-env=DEVKNX_COMMIT={commit}");
    println!("cargo:rustc-env=DEVKNX_DIRTY={dirty}");
    println!("cargo:rustc-env=DEVKNX_BUILD_STAMP={stamp}");
}
