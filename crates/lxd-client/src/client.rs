// SPDX-License-Identifier: AGPL-3.0-or-later

//! Raw HTTP transport over the LXD REST API.
//!
//! Opens a fresh HTTP/1.1 connection per request, over either a Unix domain
//! socket (local LXD snap) or HTTPS+mTLS (remote LXD cluster).

use std::fmt;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::UnixStream;

use crate::error::LxdError;
use crate::types::LxdResponse;

/// A unified raw transport stream used for WebSocket connections.
///
/// Both variants implement [`AsyncRead`] + [`AsyncWrite`] + [`Unpin`], allowing
/// `tokio_tungstenite::client_async` to work over either transport without
/// generics bubbling up into the public API.
pub(crate) enum RawStream {
    /// Local LXD daemon over a Unix domain socket.
    Unix(UnixStream),
}

impl RawStream {
    fn as_read(&mut self) -> Pin<&mut (dyn AsyncRead + Unpin)> {
        match self {
            RawStream::Unix(s) => Pin::new(s),
        }
    }

    fn as_write(&mut self) -> Pin<&mut (dyn AsyncWrite + Unpin)> {
        match self {
            RawStream::Unix(s) => Pin::new(s),
        }
    }
}

impl Unpin for RawStream {}

impl AsyncRead for RawStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.get_mut().as_read().poll_read(cx, buf)
    }
}

impl AsyncWrite for RawStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.get_mut().as_write().poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut().as_write().poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut().as_write().poll_shutdown(cx)
    }
}

/// Transport endpoint for [`LxdClient`].
#[derive(Debug, Clone)]
pub enum LxdEndpoint {
    /// Local LXD daemon over a Unix domain socket.
    UnixSocket(PathBuf),
}

/// Async client for the LXD REST API.
///
/// Opens a fresh connection per request. Use [`LxdEndpoint::UnixSocket`] for a
/// local LXD snap installation.
#[derive(Clone, Debug)]
pub struct LxdClient {
    endpoint: LxdEndpoint,
}

impl LxdClient {
    /// Creates a client for the given [`LxdEndpoint`].
    pub fn new(endpoint: LxdEndpoint) -> Result<Self, LxdError> {
        Ok(Self { endpoint })
    }

    /// WebSocket scheme for the configured endpoint (`"ws"` or `"wss"`).
    pub(crate) fn ws_scheme(&self) -> &'static str {
        match &self.endpoint {
            LxdEndpoint::UnixSocket(_) => "ws",
        }
    }

    /// Opens a raw transport connection without layering HTTP on top.
    ///
    /// Used by [`crate::events`] to hand a connected stream to
    /// `tokio_tungstenite::client_async` for the WebSocket upgrade.
    pub(crate) async fn connect_raw(&self) -> Result<(RawStream, String), LxdError> {
        match &self.endpoint {
            LxdEndpoint::UnixSocket(socket_path) => {
                let stream = UnixStream::connect(socket_path).await?;
                Ok((RawStream::Unix(stream), "localhost".to_string()))
            }
        }
    }

    /// Opens a fresh connection and returns an HTTP/1.1 sender plus the value
    /// to use for the `Host` header.
    async fn connect(
        &self,
    ) -> Result<(hyper::client::conn::http1::SendRequest<Full<Bytes>>, String), LxdError> {
        let (raw, host) = self.connect_raw().await?;
        let sender = do_handshake(TokioIo::new(raw)).await?;
        Ok((sender, host))
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
        let (mut sender, host) = self.connect().await?;

        let body_bytes = match &body {
            Some(value) => serde_json::to_vec(value)?,
            None => Vec::new(),
        };

        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", host);
        if body.is_some() {
            builder = builder.header("Content-Type", "application/json");
        }
        let request = builder.body(Full::new(Bytes::from(body_bytes)))?;

        let response = sender.send_request(request).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();

        // Check HTTP status first so a non-JSON body (e.g. a proxy 502) surfaces
        // as LxdError::Api with the real HTTP code rather than LxdError::Json.
        if !status.is_success() {
            let (status_code, message) =
                serde_json::from_slice::<LxdResponse<serde_json::Value>>(&body)
                    .ok()
                    .filter(|r| r.type_ == "error")
                    .map(|r| {
                        let msg = r.error.unwrap_or_else(|| format!("HTTP {status}"));
                        // LXD's error envelope puts the real code in `error_code`.
                        (r.error_code, msg)
                    })
                    .unwrap_or_else(|| (status.as_u16(), format!("HTTP {status}")));
            return Err(LxdError::Api {
                status_code,
                message,
            });
        }

        let parsed: LxdResponse<T> = serde_json::from_slice(&body)?;

        // LXD can return type="error" with 200 OK in edge cases.
        if parsed.type_ == "error" {
            let message = parsed
                .error
                .clone()
                .unwrap_or_else(|| format!("LXD error {}", parsed.error_code));
            return Err(LxdError::Api {
                status_code: parsed.error_code,
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
}

async fn do_handshake<IO>(
    io: IO,
) -> Result<hyper::client::conn::http1::SendRequest<Full<Bytes>>, LxdError>
where
    IO: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (sender, conn) = hyper::client::conn::http1::handshake(io).await?;
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            tracing::warn!(%err, "lxd-client: connection closed with error");
        }
    });
    Ok(sender)
}
