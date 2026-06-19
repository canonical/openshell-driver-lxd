// SPDX-License-Identifier: AGPL-3.0-or-later

//! Self-contained async HTTP client for the LXD REST API over a Unix
//! domain socket.

mod error;
mod types;

pub use error::LxdError;
pub use types::{
    Instance, InstanceState, InstanceStateCpu, InstanceStateDisk, InstanceStateMemory,
    InstanceStateNetwork, InstanceStateNetworkAddress, LxdServerInfo, Operation,
};
