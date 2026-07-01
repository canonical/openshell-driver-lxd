// SPDX-License-Identifier: AGPL-3.0-or-later

//! Raw HTTP transport over the LXD REST API Unix domain socket.
//!
//! Opens a fresh HTTP/1.1 connection per request.

use std::path::PathBuf;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::net::UnixStream;

use crate::error::LxdError;
use crate::types::LxdResponse;

/// Async client for the LXD REST API, talking exclusively over a Unix
/// domain socket.
#[derive(Debug, Clone)]
pub struct LxdClient {
    socket_path: PathBuf,
}

impl LxdClient {
    /// Creates a client for the LXD REST API socket at `socket_path`.
    ///
    /// Performs no I/O; the socket is connected fresh for every request.
    #[must_use]
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Sends a request and deserializes the LXD response envelope's
    /// `metadata` as `T`.
    ///
    /// Returns [`LxdError::Api`] for non-2xx responses or an `error`-typed
    /// envelope.
    pub(crate) async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<LxdResponse<T>, LxdError> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;

        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                tracing::warn!(%err, "lxd-client: connection closed with error");
            }
        });

        let body_bytes = match &body {
            Some(value) => serde_json::to_vec(value)?,
            None => Vec::new(),
        };

        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "localhost");
        if body.is_some() {
            builder = builder.header("Content-Type", "application/json");
        }
        let request = builder.body(Full::new(Bytes::from(body_bytes)))?;

        let response = sender.send_request(request).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();

        let parsed: LxdResponse<T> = serde_json::from_slice(&body)?;

        if parsed.type_ == "error" || !status.is_success() {
            let message = parsed
                .error
                .clone()
                .unwrap_or_else(|| format!("HTTP {status}"));
            // LXD's error envelope reports the real numeric code in
            // `error_code`, not `status_code` (which is left 0 for errors).
            let status_code = if parsed.type_ == "error" {
                parsed.error_code
            } else {
                status.as_u16()
            };
            return Err(LxdError::Api {
                status_code,
                message,
            });
        }

        Ok(parsed)
    }

    pub(crate) async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<LxdResponse<T>, LxdError> {
        self.request(Method::GET, path, None).await
    }

    pub(crate) async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
    ) -> Result<LxdResponse<T>, LxdError> {
        self.request(Method::POST, path, Some(body)).await
    }

    pub(crate) async fn put<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
    ) -> Result<LxdResponse<T>, LxdError> {
        self.request(Method::PUT, path, Some(body)).await
    }

    pub(crate) async fn delete<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<LxdResponse<T>, LxdError> {
        self.request(Method::DELETE, path, None).await
    }

    /// POST raw bytes with arbitrary extra headers; parses success/failure from
    /// the LXD response envelope without requiring `metadata` to be present.
    /// Used for the file-push endpoint whose sync response has `metadata: null`.
    pub(crate) async fn post_raw(
        &self,
        path: &str,
        content_type: &str,
        extra_headers: &[(&str, &str)],
        body: Bytes,
    ) -> Result<(), LxdError> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                tracing::warn!(%err, "lxd-client: connection closed with error");
            }
        });

        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("Host", "localhost")
            .header("Content-Type", content_type);
        for (name, value) in extra_headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(Full::new(body))?;

        let response = sender.send_request(request).await?;
        let status = response.status();
        let resp_body = response.into_body().collect().await?.to_bytes();

        if !status.is_success() {
            let parsed: serde_json::Value =
                serde_json::from_slice(&resp_body).unwrap_or_default();
            let message = parsed["error"]
                .as_str()
                .unwrap_or_else(|| parsed["message"].as_str().unwrap_or("unknown error"))
                .to_string();
            return Err(LxdError::Api {
                status_code: status.as_u16(),
                message,
            });
        }
        Ok(())
    }
}
