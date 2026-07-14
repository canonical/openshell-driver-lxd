// SPDX-License-Identifier: AGPL-3.0-or-later

//! Core LXD compute driver logic, independent of the gRPC transport.

use computev1::pb::GetCapabilitiesResponse;
use lxd_client::LxdClient;

use crate::config::Config;

const DRIVER_NAME: &str = "lxd";

/// LXD compute driver.
#[derive(Debug, Clone)]
pub struct LxdComputeDriver {
    config: Config,
    lxd: LxdClient,
}

impl LxdComputeDriver {
    #[must_use]
    pub fn new(config: Config, lxd: LxdClient) -> Self {
        Self { config, lxd }
    }

    /// Report driver capabilities and defaults.
    #[must_use]
    pub fn capabilities(&self) -> GetCapabilitiesResponse {
        GetCapabilitiesResponse {
            driver_name: DRIVER_NAME.to_string(),
            driver_version: env!("CARGO_PKG_VERSION").to_string(),
            default_image: self.config.default_image.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;
    use lxd_client::LxdEndpoint;

    use super::*;
    use crate::config::DEFAULT_LXD_SOCKET;

    #[test]
    fn capabilities_reports_static_fields() {
        let config = Config::parse_from(["openshell-driver-lxd"]);
        let lxd =
            LxdClient::new(LxdEndpoint::UnixSocket(PathBuf::from(DEFAULT_LXD_SOCKET))).unwrap();
        let driver = LxdComputeDriver::new(config, lxd);

        let response = driver.capabilities();

        assert_eq!(response.driver_name, "lxd");
        assert_eq!(response.driver_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(response.default_image, "openshell-sandbox");
    }
}
