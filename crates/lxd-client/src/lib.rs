// SPDX-License-Identifier: AGPL-3.0-or-later

//! Self-contained async HTTP client for the LXD REST API, supporting both a
//! local Unix domain socket and a remote HTTPS+mTLS endpoint.

mod client;
mod error;
mod instances;
mod operations;
pub mod resources;
mod types;

pub use client::{LxdClient, LxdEndpoint};
pub use error::LxdError;
pub use types::{
    Instance, InstanceState, InstanceStateCpu, InstanceStateDisk, InstanceStateMemory,
    InstanceStateNetwork, InstanceStateNetworkAddress, LxdServerInfo, Operation,
};
