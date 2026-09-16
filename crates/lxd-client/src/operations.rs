// SPDX-License-Identifier: AGPL-3.0-or-later

//! Waiting for asynchronous LXD operations to complete.

use std::str::FromStr;
use std::time::Duration;

use futures::StreamExt;

use crate::client::LxdClient;
use crate::error::LxdError;
use crate::types::{Operation, OperationStatus};

/// Delay between retries when reconciling operation state after a
/// WebSocket disconnect or a transient REST transport error.
const RECONCILE_RETRY_INTERVAL: Duration = Duration::from_millis(100);

enum Reconciled {
    Done(Box<Result<Operation, LxdError>>),
    Pending,
}

impl LxdClient {
    /// `GET /1.0/operations/<id>` — fetch the current state of an operation
    /// without blocking.
    ///
    /// `id` is the bare UUID from [`Operation::id`], not the full path.
    pub async fn get_operation(&self, id: &str) -> Result<Operation, LxdError> {
        self.get::<Operation>(&format!("/1.0/operations/{id}"))
            .await?
            .into_metadata()
    }

    /// Block until the operation identified by `id` reaches a terminal state.
    ///
    /// Subscribes to LXD's WebSocket event stream (`/1.0/events?type=operation`)
    /// and immediately reconciles the current operation state to close the race
    /// window between subscribe and check. Falls back to the REST long-poll
    /// (`/1.0/operations/<uuid>/wait?timeout=-1`) if the WebSocket handshake
    /// fails (e.g. an older LXD that does not support WebSocket events).
    ///
    /// Callers that need a deadline should wrap this with
    /// [`tokio::time::timeout`]:
    ///
    /// ```ignore
    /// tokio::time::timeout(Duration::from_secs(60), lxd.wait_operation(id)).await??;
    /// ```
    ///
    /// `id` is the bare UUID from [`Operation::id`], not the full path.
    pub async fn wait_operation(&self, id: &str) -> Result<Operation, LxdError> {
        let full_path = format!("/1.0/operations/{id}");

        loop {
            // Open the event subscription BEFORE the reconcile check to close
            // the race where the operation completes between check and subscribe.
            let mut events = match self.subscribe_events(&["operation"]).await {
                Ok(s) => s,
                Err(LxdError::Io(_) | LxdError::WebSocket { .. }) => {
                    // WebSocket unavailable — fall back to REST long-poll.
                    return self.wait_operation_rest(id).await;
                }
                Err(e) => return Err(e),
            };

            // Reconcile: the operation may have reached a terminal state while
            // we were connecting the WebSocket.
            if let Reconciled::Done(outcome) = self.reconcile_operation(id).await? {
                return *outcome;
            }

            // Drain the event stream until we see a terminal event for this operation.
            'stream: while let Some(result) = events.next().await {
                match result {
                    Ok(event) if event.type_ == "operation" => {
                        // LXD emits the id as either a bare UUID or the full path.
                        let Some(event_id) = event.metadata["id"].as_str() else {
                            continue;
                        };
                        if event_id != id && event_id != full_path {
                            continue;
                        }
                        let status_str = event.metadata["status"].as_str().unwrap_or("");
                        let status = OperationStatus::from_str(status_str)
                            .unwrap_or_else(|_| OperationStatus::Other(status_str.to_string()));
                        if status == OperationStatus::Success {
                            // Fetch authoritative state via REST; the event payload
                            // may omit fields that Operation requires.
                            return self.get_operation(id).await;
                        }
                        if status == OperationStatus::Failure
                            || status == OperationStatus::Cancelled
                        {
                            return Err(LxdError::OperationFailed {
                                description: event.metadata["description"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string(),
                                err: event.metadata["err"].as_str().unwrap_or("").to_string(),
                            });
                        }
                    }
                    Ok(_) => {}
                    // Io/WebSocket: reconnect. Json: skip the bad frame and keep
                    // draining — a single malformed frame should not abort the wait.
                    Err(LxdError::Io(_) | LxdError::WebSocket { .. }) => break 'stream,
                    Err(LxdError::Json(e)) => {
                        tracing::warn!(%e, "malformed event frame received from LXD; skipping");
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }

            // Stream ended (cleanly or with an error); reconcile before retrying.
            if let Reconciled::Done(outcome) = self.reconcile_operation(id).await? {
                return *outcome;
            }

            // Always sleep before re-subscribing to avoid a busy-loop when LXD
            // closes the WebSocket cleanly (e.g. a server-side idle timeout).
            tokio::time::sleep(RECONCILE_RETRY_INTERVAL).await;
        }
    }

    /// Fetches the current operation state and maps it to a terminal outcome.
    ///
    /// Transient transport errors (`Io`, `Hyper`) are treated as `Pending`
    /// so the outer loop retries rather than propagating a transient failure.
    async fn reconcile_operation(&self, id: &str) -> Result<Reconciled, LxdError> {
        match self.get_operation(id).await {
            Ok(op) if op.status == OperationStatus::Success => {
                Ok(Reconciled::Done(Box::new(Ok(op))))
            }
            Ok(op)
                if op.status == OperationStatus::Failure
                    || op.status == OperationStatus::Cancelled
                    || !op.err.is_empty() =>
            {
                Ok(Reconciled::Done(Box::new(Err(LxdError::OperationFailed {
                    description: op.description,
                    err: op.err,
                }))))
            }
            Ok(_) => Ok(Reconciled::Pending),
            Err(LxdError::Io(_) | LxdError::Hyper(_)) => Ok(Reconciled::Pending),
            Err(e) => Err(e),
        }
    }

    /// REST long-poll fallback for [`Self::wait_operation`].
    ///
    /// Used when the WebSocket event subscription is unavailable.
    async fn wait_operation_rest(&self, id: &str) -> Result<Operation, LxdError> {
        loop {
            match self
                .get::<Operation>(&format!("/1.0/operations/{id}/wait?timeout=-1"))
                .await
            {
                Ok(resp) => {
                    let op = resp.into_metadata()?;
                    if op.status == OperationStatus::Failure
                        || op.status == OperationStatus::Cancelled
                        || !op.err.is_empty()
                    {
                        return Err(LxdError::OperationFailed {
                            description: op.description,
                            err: op.err,
                        });
                    }
                    return Ok(op);
                }
                Err(LxdError::Io(_) | LxdError::Hyper(_)) => {
                    match self.get_operation(id).await {
                        Ok(op) if op.status == OperationStatus::Success => return Ok(op),
                        Ok(op)
                            if op.status == OperationStatus::Failure
                                || op.status == OperationStatus::Cancelled
                                || !op.err.is_empty() =>
                        {
                            return Err(LxdError::OperationFailed {
                                description: op.description,
                                err: op.err,
                            });
                        }
                        Ok(_) => {}
                        Err(LxdError::Io(_) | LxdError::Hyper(_)) => {}
                        Err(LxdError::Api {
                            status_code: 404, ..
                        }) => {
                            return Err(LxdError::Api {
                                status_code: 404,
                                message: format!("operation {id} not found (purged by LXD)"),
                            });
                        }
                        Err(e) => return Err(e),
                    }
                    tokio::time::sleep(RECONCILE_RETRY_INTERVAL).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}
