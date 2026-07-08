// SPDX-License-Identifier: AGPL-3.0-or-later

//! Waiting for asynchronous LXD operations to complete.

use crate::client::LxdClient;
use crate::error::LxdError;
use crate::types::Operation;

impl LxdClient {
    /// `GET /1.0/operations/<id>/wait?timeout=<timeout_secs>`.
    ///
    /// `operation_id` is the bare UUID from [`crate::types::Operation::id`],
    /// not the full `/1.0/operations/<id>` path.
    pub async fn wait_operation(
        &self,
        operation_id: &str,
        timeout_secs: Option<u32>,
    ) -> Result<Operation, LxdError> {
        let path = match timeout_secs {
            Some(timeout) => format!("/1.0/operations/{operation_id}/wait?timeout={timeout}"),
            None => format!("/1.0/operations/{operation_id}/wait"),
        };

        let operation = self.get::<Operation>(&path).await?.into_metadata()?;

        if !operation.err.is_empty() {
            return Err(LxdError::OperationFailed {
                description: operation.description.clone(),
                err: operation.err.clone(),
            });
        }

        Ok(operation)
    }
}
