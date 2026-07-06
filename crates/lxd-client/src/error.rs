// SPDX-License-Identifier: AGPL-3.0-or-later

//! Error type for the `lxd-client` crate.

use thiserror::Error;

/// Errors returned by [`crate::LxdClient`].
#[derive(Debug, Error)]
pub enum LxdError {
    /// LXD responded with a non-2xx status, or an `error`-typed envelope.
    #[error("LXD API error (status {status_code}): {message}")]
    Api {
        /// HTTP status code or LXD `error_code`.
        status_code: u16,
        /// Human-readable error message from LXD.
        message: String,
    },

    /// An LXD operation reached a final state with a non-empty `err` field.
    #[error("LXD operation failed: {description}: {err}")]
    OperationFailed {
        /// The operation's `description` field (e.g. `"Stopping container"`).
        description: String,
        /// The operation's `err` field.
        err: String,
    },

    /// A resource quantity string (e.g. `"500m"`, `"512Mi"`) could not be
    /// converted into LXD's `limits.cpu`/`limits.memory` format.
    #[error("invalid resource quantity {quantity:?}: {reason}")]
    InvalidQuantity {
        /// The original quantity string that was rejected.
        quantity: String,
        /// Why the quantity was rejected.
        reason: String,
    },

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

    /// A TLS certificate, key, or configuration error.
    #[error("TLS error: {reason}")]
    Tls {
        /// Why the TLS setup failed.
        reason: String,
    },

    /// A WebSocket-level error (framing, protocol, or handshake failure).
    #[error("WebSocket error: {reason}")]
    WebSocket {
        /// Why the WebSocket operation failed.
        reason: String,
    },
}
