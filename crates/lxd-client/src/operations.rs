// SPDX-License-Identifier: AGPL-3.0-or-later

//! Waiting for asynchronous LXD operations to complete.

use crate::client::LxdClient;
use crate::error::LxdError;
use crate::types::Operation;

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
    /// Uses `?timeout=-1` so LXD blocks indefinitely server-side. Callers that
    /// need a deadline should wrap this with [`tokio::time::timeout`]:
    ///
    /// ```ignore
    /// tokio::time::timeout(Duration::from_secs(60), lxd.wait_operation(id)).await??;
    /// ```
    ///
    /// `id` is the bare UUID from [`Operation::id`], not the full path.
    pub async fn wait_operation(&self, id: &str) -> Result<Operation, LxdError> {
        let op = self
            .get::<Operation>(&format!("/1.0/operations/{id}/wait?timeout=-1"))
            .await?
            .into_metadata()?;

        if !op.err.is_empty() {
            return Err(LxdError::OperationFailed {
                description: op.description,
                err: op.err,
            });
        }

        Ok(op)
    }
}
