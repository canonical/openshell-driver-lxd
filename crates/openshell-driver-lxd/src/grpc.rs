// SPDX-License-Identifier: AGPL-3.0-or-later

//! `ComputeDriverService` — thin tonic trait implementation that delegates
//! to [`LxdComputeDriver`] and maps [`DriverError`] to [`Status`].

use std::pin::Pin;

use computev1::pb::compute_driver_server::ComputeDriver;
use computev1::pb::{
    CreateSandboxRequest, CreateSandboxResponse, DeleteSandboxRequest, DeleteSandboxResponse,
    GetCapabilitiesRequest, GetCapabilitiesResponse, GetSandboxRequest, GetSandboxResponse,
    ListSandboxesRequest, ListSandboxesResponse, StopSandboxRequest, StopSandboxResponse,
    ValidateSandboxCreateRequest, ValidateSandboxCreateResponse, WatchSandboxesEvent,
    WatchSandboxesRequest,
};
use futures::Stream;
use tonic::{Request, Response, Status};

use crate::driver::LxdComputeDriver;
use crate::error::DriverError;

#[derive(Debug, Clone)]
pub struct ComputeDriverService {
    driver: LxdComputeDriver,
}

impl ComputeDriverService {
    #[must_use]
    pub fn new(driver: LxdComputeDriver) -> Self {
        Self { driver }
    }
}

#[tonic::async_trait]
impl ComputeDriver for ComputeDriverService {
    async fn get_capabilities(
        &self,
        _request: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<GetCapabilitiesResponse>, Status> {
        Ok(Response::new(self.driver.capabilities()))
    }

    async fn validate_sandbox_create(
        &self,
        _request: Request<ValidateSandboxCreateRequest>,
    ) -> Result<Response<ValidateSandboxCreateResponse>, Status> {
        Err(DriverError::Unimplemented("validate_sandbox_create").into())
    }

    async fn get_sandbox(
        &self,
        _request: Request<GetSandboxRequest>,
    ) -> Result<Response<GetSandboxResponse>, Status> {
        Err(DriverError::Unimplemented("get_sandbox").into())
    }

    async fn list_sandboxes(
        &self,
        _request: Request<ListSandboxesRequest>,
    ) -> Result<Response<ListSandboxesResponse>, Status> {
        Err(DriverError::Unimplemented("list_sandboxes").into())
    }

    async fn create_sandbox(
        &self,
        _request: Request<CreateSandboxRequest>,
    ) -> Result<Response<CreateSandboxResponse>, Status> {
        Err(DriverError::Unimplemented("create_sandbox").into())
    }

    async fn stop_sandbox(
        &self,
        _request: Request<StopSandboxRequest>,
    ) -> Result<Response<StopSandboxResponse>, Status> {
        Err(DriverError::Unimplemented("stop_sandbox").into())
    }

    async fn delete_sandbox(
        &self,
        _request: Request<DeleteSandboxRequest>,
    ) -> Result<Response<DeleteSandboxResponse>, Status> {
        Err(DriverError::Unimplemented("delete_sandbox").into())
    }

    type WatchSandboxesStream =
        Pin<Box<dyn Stream<Item = Result<WatchSandboxesEvent, Status>> + Send>>;

    async fn watch_sandboxes(
        &self,
        _request: Request<WatchSandboxesRequest>,
    ) -> Result<Response<Self::WatchSandboxesStream>, Status> {
        Err(DriverError::Unimplemented("watch_sandboxes").into())
    }
}
