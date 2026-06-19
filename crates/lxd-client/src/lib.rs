// SPDX-License-Identifier: AGPL-3.0-or-later

//! Self-contained async HTTP client for the LXD REST API over a Unix
//! domain socket.

mod client;
mod error;
mod instances;
pub mod resources;
mod types;

pub use client::LxdClient;
pub use error::LxdError;
pub use types::{
    Instance, InstanceState, InstanceStateCpu, InstanceStateDisk, InstanceStateMemory,
    InstanceStateNetwork, InstanceStateNetworkAddress, LxdServerInfo, Operation,
};
