// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Display-only colour decisions. Capture data and machine-readable output stay plain.

use std::io::IsTerminal as _;
use std::sync::OnceLock;

use clap::ValueEnum;
#[cfg(any(feature = "gui", feature = "tui", test))]
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Sent,
    #[cfg(any(feature = "tui", test))]
    Warning,
    #[cfg(any(feature = "tui", test))]
    Error,
}

static CLI_MODE: OnceLock<ColorMode> = OnceLock::new();

pub fn set_cli_mode(mode: ColorMode) {
    let _ = CLI_MODE.set(mode);
}

fn mode() -> ColorMode {
    *CLI_MODE.get().unwrap_or(&ColorMode::Auto)
}

fn no_color_requested() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
}

#[must_use]
pub fn terminal_enabled() -> bool {
    terminal_enabled_for(
        mode(),
        std::io::stdout().is_terminal(),
        no_color_requested(),
    )
}

#[must_use]
pub const fn terminal_enabled_for(mode: ColorMode, tty: bool, no_color: bool) -> bool {
    match mode {
        ColorMode::Auto => tty && !no_color,
        ColorMode::Always => true,
        ColorMode::Never => false,
    }
}

#[must_use]
#[cfg(any(feature = "gui", feature = "tui"))]
pub fn ui_enabled() -> bool {
    ui_enabled_for(mode(), load_preference(), no_color_requested())
}

#[must_use]
#[cfg(any(feature = "gui", feature = "tui", test))]
pub const fn ui_enabled_for(mode: ColorMode, preference: bool, no_color: bool) -> bool {
    match mode {
        ColorMode::Auto => preference && !no_color,
        ColorMode::Always => true,
        ColorMode::Never => false,
    }
}

#[must_use]
#[cfg(any(feature = "gui", feature = "tui"))]
pub fn ui_is_locked() -> bool {
    mode() != ColorMode::Auto || no_color_requested()
}

#[must_use]
pub fn direction_tone(direction: &str) -> Tone {
    if direction == "sent" {
        Tone::Sent
    } else {
        Tone::Plain
    }
}

#[must_use]
#[cfg(any(feature = "tui", test))]
pub fn notice_tone(text: &str) -> Tone {
    if text.starts_with("Router ") && text.contains("lost routing frames") {
        Tone::Error
    } else if text.starts_with("Local ") && text.contains("subscriber missed") {
        Tone::Warning
    } else {
        Tone::Plain
    }
}

#[must_use]
pub fn ansi(text: &str, tone: Tone, enabled: bool) -> String {
    let code = match tone {
        Tone::Plain => return text.to_owned(),
        Tone::Sent => "36",
        #[cfg(any(feature = "tui", test))]
        Tone::Warning => "33",
        #[cfg(any(feature = "tui", test))]
        Tone::Error => "31",
    };
    if enabled {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Style only the known direction field; keep the byte-for-byte text when disabled.
#[must_use]
pub fn capture_line(text: &str, direction: &str, enabled: bool) -> String {
    let tone = direction_tone(direction);
    if !enabled || tone == Tone::Plain {
        return text.to_owned();
    }
    let field = format!("direction={direction}");
    text.replacen(&field, &ansi(&field, tone, true), 1)
}

#[cfg(any(feature = "gui", feature = "tui", test))]
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct DisplayPreference {
    color: Option<bool>,
}

#[cfg(any(feature = "gui", feature = "tui"))]
fn preference_path() -> Result<std::path::PathBuf, String> {
    Ok(crate::paths::data_dir()?.join("display.json"))
}

#[must_use]
#[cfg(any(feature = "gui", feature = "tui"))]
pub fn load_preference() -> bool {
    preference_path()
        .ok()
        .is_none_or(|path| load_preference_from(&path))
}

#[cfg(any(feature = "gui", feature = "tui", test))]
fn load_preference_from(path: &std::path::Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<DisplayPreference>(&bytes).ok())
        .and_then(|preference| preference.color)
        .unwrap_or(true)
}

#[cfg(any(feature = "gui", feature = "tui"))]
pub fn save_preference(enabled: bool) -> Result<(), String> {
    let path = preference_path()?;
    save_preference_to(&path, enabled)
}

#[cfg(any(feature = "gui", feature = "tui", test))]
fn save_preference_to(path: &std::path::Path, enabled: bool) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let content = serde_json::to_vec_pretty(&DisplayPreference {
        color: Some(enabled),
    })
    .map_err(|error| error.to_string())?;
    std::fs::write(path, content).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_respects_terminal_and_no_color() {
        assert!(terminal_enabled_for(ColorMode::Auto, true, false));
        assert!(!terminal_enabled_for(ColorMode::Auto, false, false));
        assert!(!terminal_enabled_for(ColorMode::Auto, true, true));
        assert!(terminal_enabled_for(ColorMode::Always, false, true));
        assert!(!terminal_enabled_for(ColorMode::Never, true, false));
    }

    #[test]
    fn ui_starts_on_and_can_be_overridden() {
        assert!(ui_enabled_for(ColorMode::Auto, true, false));
        assert!(!ui_enabled_for(ColorMode::Auto, false, false));
        assert!(!ui_enabled_for(ColorMode::Auto, true, true));
    }

    #[test]
    fn capture_text_changes_only_when_requested() {
        let line = "id=1 direction=sent source=1.1.1";
        assert_eq!(capture_line(line, "sent", false), line);
        assert_eq!(capture_line(line, "received", true), line);
        assert_eq!(
            capture_line(line, "sent", true),
            "id=1 \x1b[36mdirection=sent\x1b[0m source=1.1.1"
        );
    }

    #[test]
    fn loss_kinds_remain_distinct() {
        assert_eq!(
            notice_tone("Router 1.1.1 reported 2 lost routing frames"),
            Tone::Error
        );
        assert_eq!(
            notice_tone("Local Capture subscriber missed 2 events"),
            Tone::Warning
        );
        assert_eq!(notice_tone("Connected"), Tone::Plain);
    }

    #[test]
    fn gui_preference_round_trips_without_a_capture_database() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("display.json");
        assert!(load_preference_from(&path));
        save_preference_to(&path, false).unwrap();
        assert!(!load_preference_from(&path));
        save_preference_to(&path, true).unwrap();
        assert!(load_preference_from(&path));
    }
}
