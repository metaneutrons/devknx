// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Per-user settings in `settings.json` in the data directory.
//!
//! Unlike `connection.json`, which belongs to one capture database, these
//! settings apply to the daemon and every client of the current user. The file
//! is optional; a missing file means the defaults.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lifecycle::IdleTimeout;

/// The settings file's name in the data directory.
pub const FILE_NAME: &str = "settings.json";

/// What `settings.json` holds. Unknown fields are kept out of the way rather
/// than rejected, so an older build can read a newer build's file.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Settings {
    /// How long the daemon waits, idle, before it stops by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout: Option<IdleTimeout>,
    /// What quitting the window or the terminal monitor does to the daemon.
    #[serde(default)]
    pub on_quit: OnQuit,
}

/// What quitting the window or the terminal monitor does to the daemon.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OnQuit {
    /// Ask each time.
    #[default]
    Ask,
    /// Stop it.
    Stop,
    /// Leave it running.
    Keep,
}

impl Settings {
    /// The settings file's path.
    ///
    /// # Errors
    ///
    /// Returns an error when the data directory cannot be located.
    pub fn path() -> Result<PathBuf, String> {
        crate::paths::data_dir().map(|dir| dir.join(FILE_NAME))
    }

    /// Read the settings; a missing file gives the defaults.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or parsed.
    pub fn load() -> Result<Self, String> {
        Self::load_from(&Self::path()?)
    }

    /// Read the settings from `path`; a missing file gives the defaults.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read or parsed.
    pub fn load_from(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("{} is not valid: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("cannot read {}: {error}", path.display())),
        }
    }

    /// Remember the answer to the quit question in `path`, keeping every
    /// other field of the file as it is.
    ///
    /// # Errors
    ///
    /// Returns an error when the file is not a JSON object, or it cannot be
    /// written.
    pub fn save_on_quit(path: &Path, on_quit: OnQuit) -> Result<(), String> {
        let mut document = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
                .map_err(|error| format!("{} is not valid: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                serde_json::Value::Object(serde_json::Map::new())
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let object = document
            .as_object_mut()
            .ok_or_else(|| format!("{} is not a JSON object", path.display()))?;
        object.insert(
            "on_quit".to_owned(),
            serde_json::to_value(on_quit).map_err(|error| error.to_string())?,
        );
        let text = serde_json::to_string_pretty(&document).map_err(|error| error.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        // Written beside the file and renamed over it, so a reader never sees
        // half of it.
        let partial = path.with_extension("json.partial");
        std::fs::write(&partial, text + "\n")
            .and_then(|()| std::fs::rename(&partial, path))
            .map_err(|error| format!("cannot write {}: {error}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read_the_idle_timeout_and_tolerate_unknown_fields() {
        let settings: Settings =
            serde_json::from_str(r#"{"idle_timeout":"2h","later":true}"#).unwrap();
        assert_eq!(settings.idle_timeout, Some("2h".parse().unwrap()));
        assert_eq!(
            serde_json::from_str::<Settings>("{}").unwrap(),
            Settings::default()
        );
        assert_eq!(settings.on_quit, OnQuit::Ask);
        // Counter-probe: a malformed period is an error, not the default.
        assert!(serde_json::from_str::<Settings>(r#"{"idle_timeout":"2 days"}"#).is_err());
    }

    #[test]
    fn the_quit_answer_is_saved_without_touching_other_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("devknx").join(FILE_NAME);
        Settings::save_on_quit(&path, OnQuit::Keep).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap().on_quit, OnQuit::Keep);

        std::fs::write(&path, r#"{"idle_timeout":"1h","later":[1,2]}"#).unwrap();
        Settings::save_on_quit(&path, OnQuit::Stop).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["idle_timeout"], "1h");
        assert_eq!(saved["later"], serde_json::json!([1, 2]));
        assert_eq!(saved["on_quit"], "stop");

        // Counter-probe: a file that is not an object is left alone.
        std::fs::write(&path, "[]").unwrap();
        assert!(Settings::save_on_quit(&path, OnQuit::Keep).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
    }
}
