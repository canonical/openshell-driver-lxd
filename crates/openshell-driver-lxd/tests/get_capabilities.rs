// SPDX-License-Identifier: AGPL-3.0-or-later

use clap::Parser;
use computev1::pb::compute_driver_server::ComputeDriver;
use computev1::pb::GetCapabilitiesRequest;
use openshell_driver_lxd::config::Config;
use openshell_driver_lxd::driver::LxdComputeDriver;
use openshell_driver_lxd::grpc::ComputeDriverService;
use tonic::Request;

#[tokio::test]
async fn get_capabilities_returns_driver_info() {
    let config = Config::parse_from(["openshell-driver-lxd"]);
    let service = ComputeDriverService::new(LxdComputeDriver::new(config));

    let response = service
        .get_capabilities(Request::new(GetCapabilitiesRequest {}))
        .await
        .expect("get_capabilities should succeed")
        .into_inner();

    assert_eq!(response.driver_name, "lxd");
    assert_eq!(response.driver_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(response.default_image, "ubuntu:24.04");
    assert!(!response.supports_gpu);
}

#[tokio::test]
async fn get_capabilities_reflects_gpu_support_flag() {
    let config = Config::parse_from(["openshell-driver-lxd", "--gpu-support"]);
    let service = ComputeDriverService::new(LxdComputeDriver::new(config));

    let response = service
        .get_capabilities(Request::new(GetCapabilitiesRequest {}))
        .await
        .expect("get_capabilities should succeed")
        .into_inner();

    assert!(response.supports_gpu);
}
