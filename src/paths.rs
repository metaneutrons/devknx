// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! One deterministic capture location for each KNXnet/IP endpoint.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use knx_rs_ip::{ConnectionSpec, parse_url};

/// The platform's private per-user application data directory.
///
/// # Errors
///
/// Returns an error when the platform's user data directory cannot be located.
pub fn data_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support"));
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/share"))
        });
    base.map(|path| path.join("devknx"))
        .ok_or_else(|| "Cannot find the current user's application data directory".into())
}

/// Canonicalize the connection URL before using it as a capture identity.
///
/// # Errors
///
/// Returns an error when the endpoint URL is invalid.
pub fn canonical_endpoint(endpoint: &str) -> Result<String, String> {
    let spec = parse_url(endpoint.trim()).map_err(|error| error.to_string())?;
    Ok(match spec {
        ConnectionSpec::Tunnel(address) => format!("tunnel://{address}"),
        ConnectionSpec::Router(address) => format!("router://{address}"),
    })
}

/// The default capture database for a connection; this function does not create it.
///
/// # Errors
///
/// Returns an error when the endpoint is invalid or no user data directory is available.
pub fn database_for_endpoint(endpoint: &str) -> Result<PathBuf, String> {
    let spec = parse_url(endpoint.trim()).map_err(|error| error.to_string())?;
    let (kind, address) = match spec {
        ConnectionSpec::Tunnel(address) => ("tunnel", address),
        ConnectionSpec::Router(address) => ("router", address),
    };
    // IP text contains neither '_' nor '-', so this replacement is injective
    // for canonical SocketAddr values and remains readable in diagnostics.
    let ip = address.ip().to_string().replace(':', "_");
    Ok(data_dir()?
        .join("captures")
        .join(format!("{kind}-{ip}-{}.sqlite", address.port())))
}

/// The pre-connection single database is retained for manual opening only.
///
/// # Errors
///
/// Returns an error when no user data directory is available.
pub fn legacy_database() -> Result<PathBuf, String> {
    Ok(data_dir()?.join("captures.sqlite"))
}

/// Last-used connection for the interactive GUI; never selects a CLI write target.
///
/// # Errors
///
/// Returns an error when no user data directory is available.
pub fn recent_connection_file() -> Result<PathBuf, String> {
    Ok(data_dir()?.join("connection.json"))
}

/// Settings tied to one database, including a manually overridden path.
#[must_use]
pub fn connection_settings_file(database: &Path) -> PathBuf {
    let mut name = OsString::from(database.as_os_str());
    name.push(".connection.json");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_paths_are_canonical_distinct_and_side_effect_free() {
        let first = database_for_endpoint("tunnel://[2001:db8::1]:3671").unwrap();
        let equivalent = database_for_endpoint("tunnel://[2001:0db8:0:0:0:0:0:1]:3671").unwrap();
        assert_eq!(first, equivalent);
        assert!(
            first
                .to_string_lossy()
                .contains("tunnel-2001_db8__1-3671.sqlite")
        );
        assert_ne!(
            first,
            database_for_endpoint("tunnel://[2001:db8::1]:3672").unwrap()
        );
        assert_ne!(
            database_for_endpoint("router://224.0.23.12:3671").unwrap(),
            database_for_endpoint("router://224.0.23.13:3671").unwrap()
        );
        assert_eq!(
            canonical_endpoint(" tunnel://[2001:0db8::1]:3671 ").unwrap(),
            "tunnel://[2001:db8::1]:3671"
        );
        assert_ne!(
            connection_settings_file(Path::new("a.sqlite")),
            connection_settings_file(Path::new("b.sqlite"))
        );
    }
}
