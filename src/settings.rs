// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Per-user settings in `settings.json` in the data directory.
//!
//! Unlike `connection.json`, which belongs to one capture database, these
//! settings apply to the daemon and every client of the current user. The file
//! is optional; a missing file means the defaults.

use std::path::PathBuf;

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
        let path = Self::path()?;
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("{} is not valid: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("cannot read {}: {error}", path.display())),
        }
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
        // Counter-probe: a malformed period is an error, not the default.
        assert!(serde_json::from_str::<Settings>(r#"{"idle_timeout":"2 days"}"#).is_err());
    }
}
