// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Small, isolated `AppKit` menu bridge for the native desktop interface.

/// GUI command requested by the macOS menu bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum MenuAction {
    /// Show the capture database picker.
    OpenDatabase = 1,
    /// Show export controls.
    ExportCsv = 2,
    /// Search for KNXnet/IP gateways.
    Discover = 4,
    /// Focus the capture filter.
    FocusFilter = 8,
    /// Show the group-read controls.
    Read = 16,
    /// Show the typed-write controls.
    Write = 32,
    /// Connect or disconnect the KNX capture owner.
    ToggleConnection = 64,
    /// Show gateway and storage settings.
    ConnectionSettings = 128,
}

impl MenuAction {
    /// Every action has a GUI consumer and a native selector.
    #[cfg(test)]
    pub const ALL: &[Self] = &[
        Self::OpenDatabase,
        Self::ExportCsv,
        Self::Discover,
        Self::FocusFilter,
        Self::Read,
        Self::Write,
        Self::ToggleConnection,
        Self::ConnectionSettings,
    ];
}

#[cfg(target_os = "macos")]
mod macos {
    use super::MenuAction;

    // SAFETY: `resources/macos.m` is compiled into this binary. It copies the
    // version string and PNG bytes synchronously and retains no Rust pointer.
    // Both functions are called on the AppKit main thread only.
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn devknx_init_macos_app(version: *const std::ffi::c_char, icon: *const u8, len: usize);
        fn devknx_take_menu_action(action: u32) -> bool;
        fn devknx_macos_menu_installed() -> bool;
    }

    pub fn init_app() {
        let version = std::ffi::CString::new(env!("CARGO_PKG_VERSION")).expect("valid version");
        let icon = include_bytes!("../resources/png/devknx-256.png");
        // SAFETY: `version` and static `icon` outlive the synchronous call.
        #[allow(unsafe_code)]
        unsafe {
            devknx_init_macos_app(version.as_ptr(), icon.as_ptr(), icon.len());
        }
    }

    pub fn take(action: MenuAction) -> bool {
        // SAFETY: a main-thread scalar call into the compiled AppKit bridge.
        #[allow(unsafe_code)]
        unsafe {
            devknx_take_menu_action(action as u32)
        }
    }

    pub fn menu_installed() -> bool {
        // SAFETY: a main-thread query of objects owned by the running AppKit app.
        #[allow(unsafe_code)]
        unsafe {
            devknx_macos_menu_installed()
        }
    }
}

/// Initialize the native app menu and About panel, if this is macOS.
// The non-macOS body is inert; marking this wrapper const would break AppKit.
#[allow(clippy::missing_const_for_fn)]
pub fn init_app() {
    #[cfg(target_os = "macos")]
    macos::init_app();
}

/// Consume a pending menu action once, if this is macOS.
#[must_use]
#[allow(clippy::missing_const_for_fn)]
pub fn take(action: MenuAction) -> bool {
    #[cfg(target_os = "macos")]
    return macos::take(action);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = action;
        false
    }
}

/// Check the actual running application menu after the window framework starts.
#[must_use]
#[allow(clippy::missing_const_for_fn)]
pub fn menu_installed() -> bool {
    #[cfg(target_os = "macos")]
    return macos::menu_installed();
    #[cfg(not(target_os = "macos"))]
    true
}

#[cfg(test)]
mod tests {
    use super::MenuAction;

    #[test]
    fn every_native_request_has_a_gui_consumer() {
        let gui = include_str!("gui.rs");
        let native = include_str!("../resources/macos.m");
        for (action, selector) in [
            (MenuAction::OpenDatabase, "openDatabase:"),
            (MenuAction::ExportCsv, "exportCsv:"),
            (MenuAction::Discover, "discover:"),
            (MenuAction::FocusFilter, "focusFilter:"),
            (MenuAction::Read, "readGroup:"),
            (MenuAction::Write, "writeGroup:"),
            (MenuAction::ToggleConnection, "toggleConnection:"),
            (MenuAction::ConnectionSettings, "connectionSettings:"),
        ] {
            assert!(MenuAction::ALL.contains(&action));
            assert!(native.contains(&format!("@selector({selector})")));
            assert!(gui.contains(&format!("MenuAction::{action:?}")));
        }
    }
}
