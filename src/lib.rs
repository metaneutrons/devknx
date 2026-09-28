// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Shared KNX capture model used by the application interfaces.

pub mod api;
pub mod capabilities;
pub mod capture;
pub mod control;
pub mod daemon;
pub mod enrichment;
pub mod ets;
pub mod ipc;
pub mod mcp;
pub mod operations;
pub mod paths;
pub mod service;
pub mod storage;
#[cfg(windows)]
mod windows_pipe;
