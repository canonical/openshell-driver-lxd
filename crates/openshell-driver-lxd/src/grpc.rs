// SPDX-License-Identifier: AGPL-3.0-or-later

//! `ComputeDriverService` — thin tonic trait implementation that delegates
//! to [`LxdComputeDriver`] and maps [`DriverError`] to [`Status`].

use std::pin::Pin;

use computev1::pb::compute_driver_server::ComputeDriver;
use computev1::pb::{
    CreateSandboxRequest, CreateSandboxResponse, DeleteSandboxRequest, DeleteSandboxResponse,
    GetCapabilitiesRequest, GetCapabilitiesResponse, GetSandboxRequest, GetSandboxResponse,
    ListSandboxesRequest, ListSandboxesResponse, StopSandboxRequest, StopSandboxResponse,
    ValidateSandboxCreateRequest, ValidateSandboxCreateResponse, WatchSandboxesDeletedEvent,
    WatchSandboxesEvent, WatchSandboxesRequest, watch_sandboxes_event,
};
use futures::Stream;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};

use crate::driver::LxdComputeDriver;
use crate::error::DriverError;

#[derive(Debug, Clone)]
pub struct ComputeDriverService {
    driver: LxdComputeDriver,
    // Published on every successful DeleteSandbox so WatchSandboxes can emit
    // Deleted events and the gateway immediately removes the sandbox from its store.
    deletion_tx: broadcast::Sender<String>,
}

impl ComputeDriverService {
    #[must_use]
    pub fn new(driver: LxdComputeDriver) -> Self {
        let (deletion_tx, _) = broadcast::channel(64);
        Self { driver, deletion_tx }
    }
}

/// Resolves the LXD instance name to operate on: prefer `sandbox_name`
/// (what the driver's LXD calls are actually keyed on), falling back to
/// `sandbox_id` if it's empty. There's no `sandbox_id -> name` index, so a
/// request carrying only an unrelated `sandbox_id` can't be resolved any
/// other way.
fn resolve_name<'a>(sandbox_name: &'a str, sandbox_id: &'a str) -> Result<&'a str, Status> {
    if !sandbox_name.is_empty() {
        Ok(sandbox_name)
    } else if !sandbox_id.is_empty() {
        Ok(sandbox_id)
    } else {
        Err(
            DriverError::InvalidArgument("sandbox_name or sandbox_id is required".to_string())
                .into(),
        )
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
        let sandbox_id = req.sandbox_id.clone();
        let name = resolve_name(&req.sandbox_name, &req.sandbox_id)?;
        let deleted = self.driver.delete_sandbox(name).await?;
        // Publish a Deleted event so any active WatchSandboxes subscribers
        // immediately notify the gateway, which then removes the sandbox from its
        // store without waiting for the 60-second reconcile cycle.
        if deleted && !sandbox_id.is_empty() {
            let _ = self.deletion_tx.send(sandbox_id);
        }
        Ok(Response::new(DeleteSandboxResponse { deleted }))
    }

    type WatchSandboxesStream =
        Pin<Box<dyn Stream<Item = Result<WatchSandboxesEvent, Status>> + Send>>;

    async fn watch_sandboxes(
        &self,
        _request: Request<WatchSandboxesRequest>,
    ) -> Result<Response<Self::WatchSandboxesStream>, Status> {
        // Subscribe to the deletion broadcast channel and map each sandbox_id
        // to a WatchSandboxesDeletedEvent so the gateway immediately removes
        // deleted sandboxes from its store (rather than waiting for the 60s
        // reconcile). Lagged messages are discarded — the gateway's reconcile
        // is the safety net for any events missed during a restart.
        let rx = self.deletion_tx.subscribe();
        let stream = BroadcastStream::new(rx)
            .filter_map(|result| {
                result.ok().map(|sandbox_id| {
                    Ok(WatchSandboxesEvent {
                        payload: Some(watch_sandboxes_event::Payload::Deleted(
                            WatchSandboxesDeletedEvent { sandbox_id },
                        )),
                    })
                })
            });
        Ok(Response::new(Box::pin(stream)))
    }
}
