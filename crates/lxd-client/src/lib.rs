// SPDX-License-Identifier: AGPL-3.0-or-later

//! Self-contained async HTTP client for the LXD REST API, supporting both a
//! local Unix domain socket and a remote HTTPS+mTLS endpoint.

mod acls;
mod client;
mod error;
mod events;
mod images;
mod instances;
mod networks;
mod operations;
pub mod resources;
pub mod storage;
mod types;

pub use acls::{AclAction, AclProtocol, AclState, LxdNetworkAclRule};
pub use client::{LxdClient, LxdEndpoint, LxdHttpsConfig};
pub use error::LxdError;
pub use types::{
    Instance, InstanceState, InstanceStateCpu, InstanceStateDisk, InstanceStateMemory,
    InstanceStateNetwork, InstanceStateNetworkAddress, LxdEvent, LxdServerInfo, Network, Operation,
};
