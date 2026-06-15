// SPDX-License-Identifier: AGPL-3.0-or-later

use thiserror::Error;
use tonic::Status;

/// Errors produced by [`crate::driver::LxdComputeDriver`].
#[derive(Debug, Error)]
pub enum DriverError {
    /// The requested RPC is not implemented yet.
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
}

impl From<DriverError> for Status {
    fn from(err: DriverError) -> Self {
        match err {
            DriverError::Unimplemented(msg) => Status::unimplemented(msg),
        }
    }
}
