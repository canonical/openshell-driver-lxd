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
}

impl From<DriverError> for Status {
    fn from(err: DriverError) -> Self {
        match err {
            DriverError::Unimplemented(msg) => Status::unimplemented(msg),
            DriverError::InvalidArgument(msg) => Status::invalid_argument(msg),
            DriverError::Lxd(LxdError::Api {
                status_code,
                message,
            }) => match status_code {
                400 => Status::invalid_argument(message),
                401 => Status::unauthenticated(message),
                403 => Status::permission_denied(message),
                404 => Status::not_found(message),
                409 => Status::aborted(message),
                _ => Status::internal(format!(
                    "LXD API error (status code {}): {}",
                    status_code, message
                )),
            },
            DriverError::Lxd(lxd_err) => Status::internal(lxd_err.to_string()),
        }
    }
}
