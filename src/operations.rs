// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! One validated preparation path for group-value frames and their previews.

use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
use knx_rs_core::apdu::{GroupValueApdu, GroupValuePayload};
use knx_rs_core::cemi::CemiFrame;
use knx_rs_core::dpt::{self, Dpt, DptValue, DptWireSize};
use knx_rs_core::message::MessageCode;
use knx_rs_core::types::Priority;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ets::{EtsGroup, parse_dpt};

/// A caller's intent. `RawWrite` is not a fallback from a failed typed write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum OperationRequest {
    /// Send a group read and await a matching response.
    Read { address_raw: u16, timeout_ms: u32 },
    /// Parse a value under an ETS-compatible DPT and send a group write.
    TypedWrite {
        address_raw: u16,
        dpt: Option<String>,
        value: String,
    },
    /// Expert operation with explicit inline or byte wire representation.
    RawWrite {
        address_raw: u16,
        payload: RawPayload,
    },
}

/// Auditable entry point for a request; callers cannot provide arbitrary text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationOrigin {
    /// Current-user local IPC, including CLI and desktop clients.
    LocalIpc,
    /// HTTP listener bound exclusively to loopback.
    RestLoopback,
    /// Authenticated HTTP listener reachable beyond loopback.
    RestRemote,
    /// Local MCP stdio tool call.
    McpStdio,
}

impl OperationOrigin {
    /// Stable audit label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalIpc => "local_ipc",
            Self::RestLoopback => "rest_loopback",
            Self::RestRemote => "rest_remote",
            Self::McpStdio => "mcp_stdio",
        }
    }
}

/// Explicit wire representation for the separately named raw operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "form", content = "value", rename_all = "snake_case")]
pub enum RawPayload {
    /// Lower six APCI bits, 0–63.
    Inline(u8),
    /// Complete octets after the two APDU header bytes.
    Bytes(Vec<u8>),
}

/// Successfully prepared request; `frame` is both the preview and send input.
#[derive(Clone, Debug)]
pub struct PreparedOperation {
    /// Original typed/read/raw intent for auditing.
    pub request: OperationRequest,
    /// Resolved DPT for a typed write, if any.
    pub dpt: Option<Dpt>,
    /// Exact cEMI frame to send.
    pub frame: CemiFrame,
}

/// A value cannot be safely prepared for transmission.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum OperationError {
    /// No declared DPT and no explicit one was supplied.
    #[error("group address has no DPT; supply an explicit supported DPT")]
    MissingDpt,
    /// More than one distinct declaration requires an explicit choice.
    #[error("group address declares multiple DPTs; choose one explicitly")]
    AmbiguousDpt,
    /// An explicit DPT contradicts the imported declaration.
    #[error("explicit DPT is not declared for this group address")]
    ConflictingDpt,
    /// DPT main group is not supported for safe runtime encoding.
    #[error("DPT has no supported group-value wire encoding")]
    UnsupportedDpt,
    /// DPT token is malformed.
    #[error("invalid DPT: {0}")]
    InvalidDpt(String),
    /// Value cannot be interpreted for the chosen DPT.
    #[error("invalid value for DPT {dpt}: {value}")]
    InvalidValue { dpt: Dpt, value: String },
    /// Raw payload is too long or invalid.
    #[error("invalid raw payload: {0}")]
    InvalidRaw(String),
    /// Core APDU or cEMI encoder rejected the frame.
    #[error("cannot encode group frame: {0}")]
    Encode(String),
    /// Read timeout must be bounded.
    #[error("read timeout must be between 1 and 30000 ms")]
    InvalidTimeout,
    /// The caller supplied ETS metadata for a different address.
    #[error("ETS metadata does not match requested group address")]
    MismatchedMetadata,
}

fn resolve_dpt(group: Option<&EtsGroup>, explicit: Option<&str>) -> Result<Dpt, OperationError> {
    let declared = group
        .map(|entry| {
            entry
                .dpts
                .iter()
                .map(|token| {
                    parse_dpt(token).map_err(|_| OperationError::InvalidDpt(token.clone()))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    let selected = if let Some(token) = explicit {
        let selected =
            parse_dpt(token).map_err(|_| OperationError::InvalidDpt(token.to_owned()))?;
        if !declared.is_empty() && !declared.contains(&selected) {
            return Err(OperationError::ConflictingDpt);
        }
        selected
    } else {
        let mut distinct = declared;
        distinct.sort_by_key(|dpt| (dpt.main, dpt.sub, dpt.index));
        distinct.dedup();
        match distinct.as_slice() {
            [] => return Err(OperationError::MissingDpt),
            [only] => *only,
            _ => return Err(OperationError::AmbiguousDpt),
        }
    };
    if matches!(selected.wire_size(), DptWireSize::Unknown)
        || selected.main == 31
        || !supported_typed_identifier(selected)
    {
        return Err(OperationError::UnsupportedDpt);
    }
    Ok(selected)
}

const fn supported_typed_identifier(dpt: Dpt) -> bool {
    if dpt.index != 0 {
        return false;
    }
    matches!(
        (dpt.main, dpt.sub),
        (1, 1 | 2 | 6)
            | (4 | 7 | 8 | 12 | 13 | 17 | 18, 1)
            | (5, 1 | 3 | 10)
            | (9, 1 | 4)
            | (14, 56)
            | (16, 0 | 1)
            | (29, 10)
    )
}

fn typed_value(dpt: Dpt, raw: &str) -> Result<DptValue, OperationError> {
    let invalid = || OperationError::InvalidValue {
        dpt,
        value: raw.to_owned(),
    };
    let parsed = raw.trim();
    match dpt.main {
        1 => match parsed.to_ascii_lowercase().as_str() {
            "true" | "1" | "on" => Ok(DptValue::Bool(true)),
            "false" | "0" | "off" => Ok(DptValue::Bool(false)),
            _ => Err(invalid()),
        },
        2 | 3 | 4 | 5 | 7 | 12 | 15 | 17 | 18 | 26 | 232 | 238 => parsed
            .parse::<u32>()
            .map(DptValue::UInt)
            .map_err(|_| invalid()),
        6 | 8 | 13 | 27 => parsed
            .parse::<i32>()
            .map(DptValue::Int)
            .map_err(|_| invalid()),
        9 | 14 => parsed
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(DptValue::Float)
            .ok_or_else(invalid),
        29 => parsed
            .parse::<i64>()
            .map(DptValue::Int64)
            .map_err(|_| invalid()),
        16 | 28 => Ok(DptValue::Text(raw.to_owned())),
        _ => Err(OperationError::UnsupportedDpt),
    }
}

fn frame(address_raw: u16, apdu: GroupValueApdu<'_>) -> Result<CemiFrame, OperationError> {
    let payload = apdu
        .try_to_bytes(0)
        .map_err(|error| OperationError::Encode(error.to_string()))?;
    CemiFrame::try_new_l_data(
        MessageCode::LDataReq,
        IndividualAddress::from_raw(0),
        DestinationAddress::Group(GroupAddress::from_raw(address_raw)),
        Priority::Low,
        &payload,
    )
    .map_err(|error| OperationError::Encode(error.to_string()))
}

/// Validate an operation and construct its exact cEMI preview/transmission frame.
///
/// `group` must be the active ETS record for `request.address_raw`; callers
/// with a persistent store should perform the lookup immediately before this call.
///
/// # Errors
///
/// Returns a DPT, value, timeout, or wire-encoding error without sending.
pub fn prepare(
    request: OperationRequest,
    group: Option<&EtsGroup>,
) -> Result<PreparedOperation, OperationError> {
    let address_raw = match &request {
        OperationRequest::Read { address_raw, .. }
        | OperationRequest::TypedWrite { address_raw, .. }
        | OperationRequest::RawWrite { address_raw, .. } => *address_raw,
    };
    if group.is_some_and(|entry| entry.address_raw != address_raw) {
        return Err(OperationError::MismatchedMetadata);
    }
    let (dpt, encoded) = match &request {
        OperationRequest::Read {
            address_raw,
            timeout_ms,
        } => {
            if !(1..=30_000).contains(timeout_ms) {
                return Err(OperationError::InvalidTimeout);
            }
            (None, frame(*address_raw, GroupValueApdu::Read)?)
        }
        OperationRequest::TypedWrite {
            address_raw,
            dpt,
            value,
        } => {
            let dpt = resolve_dpt(group, dpt.as_deref())?;
            let value = typed_value(dpt, value)?;
            let bytes = dpt::encode(dpt, &value)
                .map_err(|error| OperationError::Encode(error.to_string()))?;
            let payload = GroupValuePayload::from_dpt_encoded(dpt, &bytes)
                .map_err(|error| OperationError::Encode(error.to_string()))?;
            (
                Some(dpt),
                frame(*address_raw, GroupValueApdu::Write(payload))?,
            )
        }
        OperationRequest::RawWrite {
            address_raw,
            payload,
        } => {
            let payload = match payload {
                RawPayload::Inline(value) => GroupValuePayload::Inline(*value),
                RawPayload::Bytes(bytes) if !bytes.is_empty() && bytes.len() <= 32 => {
                    GroupValuePayload::Bytes(bytes)
                }
                RawPayload::Bytes(_) => {
                    return Err(OperationError::InvalidRaw(
                        "byte payload must contain 1–32 octets".to_owned(),
                    ));
                }
            };
            (None, frame(*address_raw, GroupValueApdu::Write(payload))?)
        }
    };
    Ok(PreparedOperation {
        request,
        dpt,
        frame: encoded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(dpts: &[&str]) -> EtsGroup {
        EtsGroup {
            address_raw: 0x0a03,
            notation: "1/2/3".to_owned(),
            name: "Fixture".to_owned(),
            description: String::new(),
            hierarchy: Vec::new(),
            dpts: dpts.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    #[test]
    fn typed_preview_uses_correct_apdu_representation_for_four_fixtures() {
        for (token, value, expected) in [
            ("1.001", "true", vec![0, 0x81]),
            ("5.010", "42", vec![0, 0x80, 42]),
            ("9.001", "21.5", vec![0, 0x80, 0x0c, 0x33]),
            ("13.001", "123456", vec![0, 0x80, 0, 1, 0xe2, 0x40]),
        ] {
            let prepared = prepare(
                OperationRequest::TypedWrite {
                    address_raw: 0x0a03,
                    dpt: Some(token.to_owned()),
                    value: value.to_owned(),
                },
                None,
            )
            .unwrap();
            assert_eq!(prepared.frame.payload(), expected, "{token}");
        }
    }

    #[test]
    fn missing_ambiguous_conflicting_and_unknown_dpt_block_typed_write() {
        let request = |dpt: Option<&str>| OperationRequest::TypedWrite {
            address_raw: 0x0a03,
            dpt: dpt.map(str::to_owned),
            value: "true".to_owned(),
        };
        assert!(matches!(
            prepare(request(None), None),
            Err(OperationError::MissingDpt)
        ));
        assert!(matches!(
            prepare(request(None), Some(&group(&["DPST-1-1", "DPST-1-6"]))),
            Err(OperationError::AmbiguousDpt)
        ));
        assert!(matches!(
            prepare(request(Some("1.002")), Some(&group(&["DPST-1-1"]))),
            Err(OperationError::ConflictingDpt)
        ));
        assert!(matches!(
            prepare(request(Some("999.001")), None),
            Err(OperationError::UnsupportedDpt)
        ));
        assert!(matches!(
            prepare(request(Some("1.999")), None),
            Err(OperationError::UnsupportedDpt)
        ));
        assert!(
            prepare(
                request(Some("1.006")),
                Some(&group(&["DPST-1-1", "DPST-1-6"]))
            )
            .is_ok()
        );
    }

    #[test]
    fn raw_write_has_a_distinct_request_and_rejects_invalid_payload() {
        let request = OperationRequest::RawWrite {
            address_raw: 0x0a03,
            payload: RawPayload::Inline(1),
        };
        let prepared = prepare(request.clone(), None).unwrap();
        assert_eq!(prepared.request, request);
        assert_eq!(prepared.frame.payload(), [0, 0x81]);
        assert!(matches!(
            prepare(
                OperationRequest::RawWrite {
                    address_raw: 0x0a03,
                    payload: RawPayload::Inline(64),
                },
                None
            ),
            Err(OperationError::Encode(_))
        ));
    }
}
