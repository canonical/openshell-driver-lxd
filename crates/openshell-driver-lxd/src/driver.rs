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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn capabilities_reports_static_fields() {
        let config = Config::parse_from(["openshell-driver-lxd"]);
        let driver = LxdComputeDriver::new(config);

        let response = driver.capabilities();

        assert_eq!(response.driver_name, "lxd");
        assert_eq!(response.driver_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(response.default_image, "ubuntu:24.04");
        assert!(!response.supports_gpu);
    }

    #[test]
    fn capabilities_reflects_gpu_support_flag() {
        let config = Config::parse_from(["openshell-driver-lxd", "--gpu-support"]);
        let driver = LxdComputeDriver::new(config);

        let response = driver.capabilities();

        assert!(response.supports_gpu);
    }
}
