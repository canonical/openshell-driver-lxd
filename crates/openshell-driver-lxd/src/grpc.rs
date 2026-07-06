// SPDX-License-Identifier: AGPL-3.0-or-later

//! `ComputeDriverService` — thin tonic trait implementation that delegates
//! to [`LxdComputeDriver`] and maps [`DriverError`] to [`Status`].

use std::pin::Pin;

use computev1::pb::compute_driver_server::ComputeDriver;
use computev1::pb::{
    watch_sandboxes_event, CreateSandboxRequest, CreateSandboxResponse, DeleteSandboxRequest,
    DeleteSandboxResponse, GetCapabilitiesRequest, GetCapabilitiesResponse, GetSandboxRequest,
    GetSandboxResponse, ListSandboxesRequest, ListSandboxesResponse, StopSandboxRequest,
    StopSandboxResponse, ValidateSandboxCreateRequest, ValidateSandboxCreateResponse,
    WatchSandboxesDeletedEvent, WatchSandboxesEvent, WatchSandboxesRequest,
};
use futures::Stream;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};

use crate::driver::LxdComputeDriver;
use crate::error::DriverError;

#[derive(Debug, Clone)]
pub struct ComputeDriverService {
    driver: LxdComputeDriver,
    /// Published on every successful DeleteSandbox so WatchSandboxes can emit
    /// Deleted events and the gateway immediately removes the sandbox from its
    /// store rather than waiting for the next reconcile cycle.
    deletion_tx: broadcast::Sender<String>,
}

impl ComputeDriverService {
    #[must_use]
    pub fn new(driver: LxdComputeDriver) -> Self {
        let (deletion_tx, _) = broadcast::channel(64);
        Self {
            driver,
            deletion_tx,
        }
    }
}

fn resolve_name<'a>(sandbox_name: &'a str, _sandbox_id: &'a str) -> Result<&'a str, Status> {
    if !sandbox_name.is_empty() {
        Ok(sandbox_name)
    } else {
        Err(DriverError::InvalidArgument("sandbox_name is required".to_string()).into())
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
        request: Request<ValidateSandboxCreateRequest>,
    ) -> Result<Response<ValidateSandboxCreateResponse>, Status> {
        let sandbox = request.into_inner().sandbox.ok_or_else(|| {
            Status::from(DriverError::InvalidArgument(
                "sandbox is required".to_string(),
            ))
        })?;
        self.driver.validate_sandbox_create(&sandbox).await?;
        Ok(Response::new(ValidateSandboxCreateResponse {}))
    }

    async fn get_sandbox(
        &self,
        request: Request<GetSandboxRequest>,
    ) -> Result<Response<GetSandboxResponse>, Status> {
        let req = request.into_inner();
        let name = resolve_name(&req.sandbox_name, &req.sandbox_id)?;
        let sandbox = self.driver.get_sandbox(name).await?;
        Ok(Response::new(GetSandboxResponse {
            sandbox: Some(sandbox),
        }))
    }

    async fn list_sandboxes(
        &self,
        _request: Request<ListSandboxesRequest>,
    ) -> Result<Response<ListSandboxesResponse>, Status> {
        let sandboxes = self.driver.list_sandboxes().await?;
        Ok(Response::new(ListSandboxesResponse { sandboxes }))
    }

    async fn create_sandbox(
        &self,
        request: Request<CreateSandboxRequest>,
    ) -> Result<Response<CreateSandboxResponse>, Status> {
        let sandbox = request.into_inner().sandbox.ok_or_else(|| {
            Status::from(DriverError::InvalidArgument(
                "sandbox is required".to_string(),
            ))
        })?;
        self.driver.create_sandbox(&sandbox).await?;
        Ok(Response::new(CreateSandboxResponse {}))
    }

    async fn stop_sandbox(
        &self,
        request: Request<StopSandboxRequest>,
    ) -> Result<Response<StopSandboxResponse>, Status> {
        let req = request.into_inner();
        let name = resolve_name(&req.sandbox_name, &req.sandbox_id)?;
        self.driver.stop_sandbox(name).await?;
        Ok(Response::new(StopSandboxResponse {}))
    }

    async fn delete_sandbox(
        &self,
        request: Request<DeleteSandboxRequest>,
    ) -> Result<Response<DeleteSandboxResponse>, Status> {
        let req = request.into_inner();
        let name = resolve_name(&req.sandbox_name, &req.sandbox_id)?;
        match self.driver.delete_sandbox(name).await? {
            Some(sandbox_id) => {
                if !sandbox_id.is_empty() {
                    self.deletion_tx.send(sandbox_id).ok();
                }
                Ok(Response::new(DeleteSandboxResponse { deleted: true }))
            }
            None => Ok(Response::new(DeleteSandboxResponse { deleted: false })),
        }
    }

    type WatchSandboxesStream =
        Pin<Box<dyn Stream<Item = Result<WatchSandboxesEvent, Status>> + Send>>;

    async fn watch_sandboxes(
        &self,
        _request: Request<WatchSandboxesRequest>,
    ) -> Result<Response<Self::WatchSandboxesStream>, Status> {
        let rx = self.deletion_tx.subscribe();
        let stream = BroadcastStream::new(rx).map(|result| match result {
            Ok(sandbox_id) => Ok(WatchSandboxesEvent {
                payload: Some(watch_sandboxes_event::Payload::Deleted(
                    WatchSandboxesDeletedEvent { sandbox_id },
                )),
            }),
            Err(BroadcastStreamRecvError::Lagged(n)) => Err(Status::data_loss(format!(
                "WatchSandboxes receiver lagged and missed {n} event(s); reconnect and re-list to resync"
            ))),
        });
        Ok(Response::new(Box::pin(stream)))
    }
}
