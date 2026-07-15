// SPDX-License-Identifier: AGPL-3.0-or-later

//! Serde types mirroring LXD's REST API JSON shapes.

use std::collections::HashMap;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::LxdError;

/// LXD sends explicit JSON `null` (not a missing key) for several
/// `InstanceState` maps when an instance is stopped. `#[serde(default)]`
/// alone only handles a missing key, not an explicit `null`, so affected
/// fields also need this helper as their `deserialize_with`.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

/// Envelope every LXD REST API response is wrapped in.
///
/// `metadata` is the typed payload for `sync` responses, and for `async`
/// responses LXD inlines the full [`Operation`] there too (never just a bare
/// ID), so callers never need a follow-up `GET` to resolve it.
///
/// `status_code` is only meaningful for `sync`/`async` responses; LXD's own
/// error responses leave it `0` and report the real numeric code in
/// `error_code`.
#[derive(Debug, Clone, Deserialize)]
pub struct LxdResponse<T> {
    #[serde(rename = "type")]
    pub type_: String,
    pub status_code: u16,
    #[serde(default)]
    pub error_code: u16,
    pub metadata: Option<T>,
    pub error: Option<String>,
}

impl<T> LxdResponse<T> {
    /// Extracts `metadata`, or [`LxdError::Api`] if LXD reported success
    /// without a payload (unexpected, but checked rather than panicking).
    pub(crate) fn into_metadata(self) -> Result<T, LxdError> {
        self.metadata.ok_or_else(|| LxdError::Api {
            status_code: self.status_code,
            message: "LXD response missing metadata".to_string(),
        })
    }
}

/// A container or virtual machine, as returned by
/// `GET /1.0/instances/<name>` and `GET /1.0/instances?recursion=1`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub name: String,
    pub description: String,
    pub status: String,
    pub status_code: u16,
    #[serde(default)]
    pub architecture: String,
    pub ephemeral: bool,
    pub profiles: Vec<String>,
    pub config: HashMap<String, String>,
    pub devices: HashMap<String, HashMap<String, String>>,
    #[serde(rename = "type")]
    pub type_: String,
    pub project: String,
}

/// Runtime state of an instance, as returned by
/// `GET /1.0/instances/<name>/state`.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceState {
    pub status: String,
    pub status_code: u16,
    /// `null` rather than `{}` when the instance is stopped.
    #[serde(default, deserialize_with = "null_to_default")]
    pub disk: HashMap<String, InstanceStateDisk>,
    pub memory: InstanceStateMemory,
    /// `null` rather than `{}` when the instance is stopped.
    #[serde(default, deserialize_with = "null_to_default")]
    pub network: HashMap<String, InstanceStateNetwork>,
    pub pid: i64,
    pub processes: i64,
    pub cpu: InstanceStateCpu,
}

/// Disk usage for one device in [`InstanceState::disk`].
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceStateDisk {
    pub usage: i64,
    #[serde(default)]
    pub total: i64,
}

/// Memory usage section of [`InstanceState`].
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceStateMemory {
    pub usage: i64,
    #[serde(default)]
    pub usage_peak: i64,
    #[serde(default)]
    pub total: i64,
    #[serde(default)]
    pub swap_usage: i64,
    #[serde(default)]
    pub swap_usage_peak: i64,
}

/// CPU usage section of [`InstanceState`].
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceStateCpu {
    pub usage: i64,
}

/// Network interface state for one device in [`InstanceState::network`].
///
/// IP addresses live in [`InstanceStateNetworkAddress::address`], nested
/// under `addresses`, not as a flat field on this struct.
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceStateNetwork {
    #[serde(default)]
    pub addresses: Vec<InstanceStateNetworkAddress>,
    #[serde(default)]
    pub hwaddr: String,
    #[serde(default)]
    pub host_name: String,
    #[serde(default)]
    pub mtu: i64,
    #[serde(default)]
    pub state: String,
    #[serde(rename = "type", default)]
    pub type_: String,
}

/// A single address entry within [`InstanceStateNetwork::addresses`].
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceStateNetworkAddress {
    pub family: String,
    pub address: String,
    #[serde(default)]
    pub netmask: String,
    #[serde(default)]
    pub scope: String,
}

/// An asynchronous background operation, returned by every mutating
/// instance endpoint and by `GET /1.0/operations/<id>/wait`.
#[derive(Debug, Clone, Deserialize)]
pub struct Operation {
    pub id: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub description: String,
    pub status: OperationStatus,
    pub status_code: u16,
    #[serde(default)]
    pub resources: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub err: String,
    #[serde(default)]
    pub location: String,
}

/// Lifecycle status of an [`Operation`], as reported by LXD's `status`
/// field (also mirrored in the `status` key of
/// `/1.0/events?type=operation` frames).
///
/// LXD reports several transient statuses beyond the ones any caller in
/// this crate inspects (`Starting`, `Stopping`, `Aborting`, ...); any
/// value that isn't one of the terminal/pending statuses handled here is
/// always non-terminal, so [`OperationStatus::Other`] captures those
/// without needing this enum to track LXD's full status list.
#[derive(Clone, Debug, Eq, PartialEq, strum::Display, strum::EnumString)]
pub enum OperationStatus {
    Pending,
    Running,
    Success,
    Failure,
    Cancelled,
    #[strum(default)]
    Other(String),
}

impl<'de> Deserialize<'de> for OperationStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

/// Server info, as returned by `GET /1.0`.
#[derive(Debug, Clone, Deserialize)]
pub struct LxdServerInfo {
    #[serde(default)]
    pub api_extensions: Vec<String>,
}

/// A single event frame from `GET /1.0/events`.
#[derive(Debug, Clone, Deserialize)]
pub struct LxdEvent {
    /// ISO-8601 timestamp of the event.
    pub timestamp: String,
    /// Event type: `"operation"`, `"lifecycle"`, `"logging"`, etc.
    #[serde(rename = "type")]
    pub type_: String,
    /// Raw event payload; shape varies by `type_`.
    pub metadata: serde_json::Value,
}
