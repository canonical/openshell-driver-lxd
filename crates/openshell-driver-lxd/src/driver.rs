// SPDX-License-Identifier: AGPL-3.0-or-later

//! Core LXD compute driver logic, independent of the gRPC transport.

use computev1::pb::GetCapabilitiesResponse;

use crate::config::Config;

const DRIVER_NAME: &str = "lxd";
const DEFAULT_SANDBOX_IMAGE: &str = "ubuntu:24.04";

/// LXD compute driver.
#[derive(Debug, Clone)]
pub struct LxdComputeDriver {
    config: Config,
}

impl LxdComputeDriver {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Report driver capabilities and defaults.
    #[must_use]
    pub fn capabilities(&self) -> GetCapabilitiesResponse {
        GetCapabilitiesResponse {
            driver_name: DRIVER_NAME.to_string(),
            driver_version: env!("CARGO_PKG_VERSION").to_string(),
            default_image: DEFAULT_SANDBOX_IMAGE.to_string(),
            supports_gpu: self.config.gpu_support,
        }
    }
}
