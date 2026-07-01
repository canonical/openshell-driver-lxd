// SPDX-License-Identifier: AGPL-3.0-or-later

use lxd_client::LxdError;
use thiserror::Error;
use tonic::Status;

/// Errors produced by [`crate::driver::LxdComputeDriver`].
#[derive(Debug, Error)]
pub enum DriverError {
    /// The requested RPC is not implemented yet.
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),

    /// The request was missing a required field or had an invalid value.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The LXD REST API call failed.
    #[error("LXD error: {0}")]
    Lxd(#[from] LxdError),

    /// An internal driver error (I/O, unexpected state, etc.).
    #[error("internal: {0}")]
    Internal(String),
}

impl From<DriverError> for Status {
    fn from(err: DriverError) -> Self {
        match err {
            DriverError::Unimplemented(msg) => Status::unimplemented(msg),
            DriverError::InvalidArgument(msg) => Status::invalid_argument(msg),
            DriverError::Lxd(LxdError::Api {
                status_code: 409,
                message,
            }) => Status::already_exists(message),
            DriverError::Lxd(LxdError::Api {
                status_code: 404,
                message,
            }) => Status::not_found(message),
            DriverError::Lxd(lxd_err) => Status::internal(lxd_err.to_string()),
            DriverError::Internal(msg) => Status::internal(msg),
        }
    }
}
