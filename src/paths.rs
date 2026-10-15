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

/// Where a copy of the capture `database` goes before it is cleared, named
/// after the capture and the moment of the copy.
///
/// # Errors
///
/// Returns an error when the data directory cannot be located.
pub fn archive_path(database: &Path, now: std::time::SystemTime) -> Result<PathBuf, String> {
    Ok(archive_path_in(&data_dir()?.join("archive"), database, now))
}

/// [`archive_path`] in a given directory.
#[must_use]
pub fn archive_path_in(directory: &Path, database: &Path, now: std::time::SystemTime) -> PathBuf {
    let stem = database
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("capture");
    directory.join(format!("{stem}-{}.sqlite", utc_stamp(now)))
}

/// `YYYYMMDD-HHMMSS` in UTC, without a date library: the daemon is built
/// without one.
fn utc_stamp(time: std::time::SystemTime) -> String {
    let seconds = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // Days since 1970-01-01 to a calendar date (the civil-from-days method).
    let shifted = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
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

    #[test]
    fn the_archive_name_carries_the_capture_and_the_moment_in_utc() {
        use std::time::{Duration, UNIX_EPOCH};
        // 2026-10-15 09:08:07 UTC and the epoch, with the values worked out by
        // `datetime` outside this code, not by the arithmetic under test.
        let moment = UNIX_EPOCH + Duration::from_secs(1_792_055_287);
        assert_eq!(utc_stamp(moment), "20261015-090807");
        assert_eq!(utc_stamp(UNIX_EPOCH), "19700101-000000");
        // A leap day, and the day after it.
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_168_461);
        assert_eq!(utc_stamp(leap), "20240229-010101");
        let after = UNIX_EPOCH + Duration::from_hours(474_792);
        assert_eq!(utc_stamp(after), "20240301-000000");
        assert_eq!(
            archive_path_in(Path::new("/a"), Path::new("/x/captures.sqlite"), moment),
            PathBuf::from("/a/captures-20261015-090807.sqlite")
        );
        assert_eq!(
            archive_path_in(Path::new("/a"), Path::new("/"), UNIX_EPOCH),
            PathBuf::from("/a/capture-19700101-000000.sqlite")
        );
    }
}
