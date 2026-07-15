// SPDX-License-Identifier: AGPL-3.0-or-later

//! WebSocket event stream subscription (`GET /1.0/events`).

use std::pin::Pin;

use futures::{Stream, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use crate::client::LxdClient;
use crate::error::LxdError;
use crate::types::LxdEvent;

/// A boxed, pinned event stream returned by [`LxdClient::subscribe_events`].
pub type EventStream = Pin<Box<dyn Stream<Item = Result<LxdEvent, LxdError>> + Send>>;

impl LxdClient {
    /// Subscribe to the LXD event stream over WebSocket.
    ///
    /// `types` is a slice of event type names to filter on (e.g.
    /// `&["operation"]`, `&["lifecycle", "operation"]`). Returns a [`Stream`]
    /// of deserialized [`LxdEvent`] frames; the stream ends when the
    /// WebSocket connection closes.
    ///
    /// The caller is responsible for bounding the stream lifetime. Use
    /// [`StreamExt::next`] inside a [`tokio::time::timeout`] to avoid waiting
    /// forever if LXD stops sending events.
    pub async fn subscribe_events(&self, types: &[&str]) -> Result<EventStream, LxdError> {
        let (raw, host) = self.connect_raw().await?;
        let scheme = self.ws_scheme();
        let type_param = types.join(",");
        let url = format!("{scheme}://{host}/1.0/events?type={type_param}");

        let (ws, _) = tokio_tungstenite::client_async(url, raw)
            .await
            .map_err(|e| match e {
                tokio_tungstenite::tungstenite::Error::Io(io) => LxdError::Io(io),
                other => LxdError::WebSocket {
                    reason: other.to_string(),
                },
            })?;

        let stream = ws.filter_map(|msg| async {
            match msg {
                Ok(Message::Text(text)) => {
                    Some(serde_json::from_str::<LxdEvent>(&text).map_err(LxdError::Json))
                }
                Ok(_) => None,
                Err(tokio_tungstenite::tungstenite::Error::Io(e)) => Some(Err(LxdError::Io(e))),
                Err(e) => Some(Err(LxdError::WebSocket {
                    reason: e.to_string(),
                })),
            }
        });

        Ok(Box::pin(stream))
    }
}
