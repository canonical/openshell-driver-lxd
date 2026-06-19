// SPDX-License-Identifier: AGPL-3.0-or-later

//! Error type for the `lxd-client` crate.

use thiserror::Error;

/// Errors returned by [`crate::LxdClient`].
#[derive(Debug, Error)]
pub enum LxdError {
    /// LXD responded with a non-2xx status, or an `error`-typed envelope.
    #[error("LXD API error (status {status_code}): {message}")]
    Api { status_code: u16, message: String },

    /// An LXD operation reached a final state with a non-empty `err` field.
    #[error("LXD operation failed: {description}: {err}")]
    OperationFailed { description: String, err: String },

    /// A resource quantity string (e.g. `"500m"`, `"512Mi"`) could not be
    /// converted into LXD's `limits.cpu`/`limits.memory` format.
    #[error("invalid resource quantity {quantity:?}: {reason}")]
    InvalidQuantity { quantity: String, reason: String },

    /// Failed to connect to, or read/write, the LXD Unix socket.
    #[error("transport error: {0}")]
    Io(#[from] std::io::Error),

    /// A hyper-level HTTP error (handshake, send, or body read failure).
    #[error("hyper error: {0}")]
    Hyper(#[from] hyper::Error),

    /// Failed to build the outgoing HTTP request.
    #[error("invalid request: {0}")]
    Http(#[from] hyper::http::Error),

    /// LXD returned a response body that wasn't valid JSON, or didn't match
    /// the expected shape.
    #[error("invalid JSON from LXD: {0}")]
    Json(#[from] serde_json::Error),
}
