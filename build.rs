// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

fn main() {
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
