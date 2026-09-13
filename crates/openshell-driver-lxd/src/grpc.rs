// SPDX-License-Identifier: AGPL-3.0-or-later

//! `ComputeDriverService` — thin tonic trait implementation that delegates
//! to [`LxdComputeDriver`] and maps [`DriverError`] to [`Status`].

use std::pin::Pin;

use computev1::pb::compute_driver_server::ComputeDriver;
use computev1::pb::DriverSandbox;
use computev1::pb::{
    watch_sandboxes_event, CreateSandboxRequest, CreateSandboxResponse, DeleteSandboxRequest,
    DeleteSandboxResponse, GetCapabilitiesRequest, GetCapabilitiesResponse, GetSandboxRequest,
    GetSandboxResponse, ListSandboxesRequest, ListSandboxesResponse, StopSandboxRequest,
    StopSandboxResponse, ValidateSandboxCreateRequest, ValidateSandboxCreateResponse,
    WatchSandboxesDeletedEvent, WatchSandboxesEvent, WatchSandboxesRequest,
    WatchSandboxesSandboxEvent,
};
use futures::Stream;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tokio_stream::StreamExt;
use tonic::{Code, Request, Response, Status};

use crate::driver::LxdComputeDriver;
use crate::error::DriverError;
use crate::watcher;

#[derive(Debug, Clone)]
pub struct ComputeDriverService {
    driver: LxdComputeDriver,
    /// Published on every successful DeleteSandbox so WatchSandboxes can emit
    /// Deleted events and the gateway immediately removes the sandbox from its
    /// store rather than waiting for the next reconcile cycle.
    deletion_tx: broadcast::Sender<String>,
    /// Published by the LXD lifecycle watcher whenever a driver-managed
    /// instance changes state, so a sandbox whose supervisor exited is
    /// reported at once instead of on the gateway's next reconcile.
    sandbox_tx: broadcast::Sender<DriverSandbox>,
}

impl ComputeDriverService {
    #[must_use]
    pub fn new(driver: LxdComputeDriver) -> Self {
        let (deletion_tx, _) = broadcast::channel(64);
        let (sandbox_tx, _) = broadcast::channel(64);
        watcher::spawn(driver.lxd_client(), sandbox_tx.clone());
        Self {
            driver,
            deletion_tx,
            sandbox_tx,
        }
    }

    /// Builds the service without the LXD lifecycle watcher, for tests that
    /// have no LXD to subscribe to.
    #[must_use]
    pub fn without_watcher(driver: LxdComputeDriver) -> Self {
        let (deletion_tx, _) = broadcast::channel(64);
        let (sandbox_tx, _) = broadcast::channel(64);
        Self {
            driver,
            deletion_tx,
            sandbox_tx,
        }
    }
}

/// Resolves the instance name a request should act on. Prefers
/// `sandbox_name` (the common case); when it's empty, falls back to
/// looking up the instance whose `user.openshell.sandbox_id` config key
/// matches `sandbox_id` — both fields exist on these requests precisely so
/// callers can address a sandbox by either.
async fn resolve_name(
    driver: &LxdComputeDriver,
    sandbox_name: &str,
    sandbox_id: &str,
) -> Result<String, Status> {
    if !sandbox_name.is_empty() {
        return Ok(sandbox_name.to_string());
    }
    if sandbox_id.is_empty() {
        return Err(DriverError::InvalidArgument(
            "sandbox_name or sandbox_id is required".to_string(),
        )
        .into());
    }
    driver
        .find_name_by_sandbox_id(sandbox_id)
        .await?
        .ok_or_else(|| {
            Status::not_found(format!("no sandbox found with sandbox_id {sandbox_id:?}"))
        })
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
        let name = resolve_name(&self.driver, &req.sandbox_name, &req.sandbox_id).await?;
        let sandbox = self.driver.get_sandbox(&name).await?;
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
        let name = resolve_name(&self.driver, &req.sandbox_name, &req.sandbox_id).await?;
        self.driver.stop_sandbox(&name).await?;
        Ok(Response::new(StopSandboxResponse {}))
    }

    async fn delete_sandbox(
        &self,
        request: Request<DeleteSandboxRequest>,
    ) -> Result<Response<DeleteSandboxResponse>, Status> {
        let req = request.into_inner();
        let name = match resolve_name(&self.driver, &req.sandbox_name, &req.sandbox_id).await {
            Ok(name) => name,
            // No instance carries this id, so there is nothing to delete —
            // the same answer an unknown name gets. Delete must be idempotent
            // however the caller addresses the sandbox.
            Err(status) if status.code() == Code::NotFound => {
                return Ok(Response::new(DeleteSandboxResponse { deleted: false }));
            }
            Err(status) => return Err(status),
        };
        match self.driver.delete_sandbox(&name).await? {
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
        let lagged = |n| {
            Status::data_loss(format!(
                "WatchSandboxes receiver lagged and missed {n} event(s); reconnect and re-list to resync"
            ))
        };

        let deleted =
            BroadcastStream::new(self.deletion_tx.subscribe()).map(move |result| match result {
                Ok(sandbox_id) => Ok(WatchSandboxesEvent {
                    payload: Some(watch_sandboxes_event::Payload::Deleted(
                        WatchSandboxesDeletedEvent { sandbox_id },
                    )),
                }),
                Err(BroadcastStreamRecvError::Lagged(n)) => Err(lagged(n)),
            });

        let updated =
            BroadcastStream::new(self.sandbox_tx.subscribe()).map(move |result| match result {
                Ok(sandbox) => Ok(WatchSandboxesEvent {
                    payload: Some(watch_sandboxes_event::Payload::Sandbox(
                        WatchSandboxesSandboxEvent {
                            sandbox: Some(sandbox),
                        },
                    )),
                }),
                Err(BroadcastStreamRecvError::Lagged(n)) => Err(lagged(n)),
            });

        Ok(Response::new(Box::pin(deleted.merge(updated))))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use clap::Parser;
    use lxd_client::{LxdClient, LxdEndpoint};

    use super::*;
    use crate::config::{Config, DEFAULT_LXD_SOCKET};

    /// None of these tests reach LXD: the client only connects when a
    /// request is sent, and every path exercised here returns before that.
    fn service() -> ComputeDriverService {
        let config = Config::parse_from(["openshell-driver-lxd"]);
        let lxd =
            LxdClient::new(LxdEndpoint::UnixSocket(PathBuf::from(DEFAULT_LXD_SOCKET))).unwrap();
        ComputeDriverService::without_watcher(LxdComputeDriver::new(config, lxd))
    }

    async fn next_event(
        stream: &mut <ComputeDriverService as ComputeDriver>::WatchSandboxesStream,
    ) -> Result<WatchSandboxesEvent, Status> {
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("watch stream should yield within 5s")
            .expect("watch stream should not end")
    }

    #[tokio::test]
    async fn resolve_name_prefers_name_without_looking_up_id() {
        let service = service();
        let name = resolve_name(&service.driver, "by-name", "some-id")
            .await
            .expect("a name needs no lookup");
        assert_eq!(name, "by-name");
    }

    #[tokio::test]
    async fn resolve_name_requires_name_or_id() {
        let service = service();
        let status = resolve_name(&service.driver, "", "")
            .await
            .expect_err("neither name nor id should be rejected");
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn requests_without_a_sandbox_are_invalid() {
        let service = service();

        let status = service
            .validate_sandbox_create(Request::new(ValidateSandboxCreateRequest { sandbox: None }))
            .await
            .expect_err("validate without sandbox should fail");
        assert_eq!(status.code(), Code::InvalidArgument);

        let status = service
            .create_sandbox(Request::new(CreateSandboxRequest { sandbox: None }))
            .await
            .expect_err("create without sandbox should fail");
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn stop_and_delete_require_name_or_id() {
        let service = service();

        let status = service
            .stop_sandbox(Request::new(StopSandboxRequest::default()))
            .await
            .expect_err("stop without identity should fail");
        assert_eq!(status.code(), Code::InvalidArgument);

        let status = service
            .delete_sandbox(Request::new(DeleteSandboxRequest::default()))
            .await
            .expect_err("delete without identity should fail");
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn watch_forwards_deletions_and_snapshots() {
        let service = service();
        let mut stream = service
            .watch_sandboxes(Request::new(WatchSandboxesRequest {}))
            .await
            .expect("watch should open")
            .into_inner();

        service.deletion_tx.send("sb-gone".to_string()).unwrap();
        match next_event(&mut stream).await.unwrap().payload {
            Some(watch_sandboxes_event::Payload::Deleted(deleted)) => {
                assert_eq!(deleted.sandbox_id, "sb-gone");
            }
            other => panic!("expected a Deleted event, got {other:?}"),
        }

        let snapshot = DriverSandbox {
            id: "sb-live".to_string(),
            name: "sb-live".to_string(),
            ..Default::default()
        };
        service.sandbox_tx.send(snapshot.clone()).unwrap();
        match next_event(&mut stream).await.unwrap().payload {
            Some(watch_sandboxes_event::Payload::Sandbox(event)) => {
                assert_eq!(event.sandbox, Some(snapshot));
            }
            other => panic!("expected a Sandbox event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_watcher_receives_every_event() {
        let service = service();
        let mut first = service
            .watch_sandboxes(Request::new(WatchSandboxesRequest {}))
            .await
            .unwrap()
            .into_inner();
        let mut second = service
            .watch_sandboxes(Request::new(WatchSandboxesRequest {}))
            .await
            .unwrap()
            .into_inner();

        service.deletion_tx.send("sb-1".to_string()).unwrap();

        for stream in [&mut first, &mut second] {
            assert!(matches!(
                next_event(stream).await.unwrap().payload,
                Some(watch_sandboxes_event::Payload::Deleted(_))
            ));
        }
    }

    /// A watcher that falls behind must be told it missed events (so the
    /// gateway reconnects and re-lists) rather than silently skipping them.
    #[tokio::test]
    async fn lagging_watcher_gets_data_loss() {
        let service = service();
        let mut stream = service
            .watch_sandboxes(Request::new(WatchSandboxesRequest {}))
            .await
            .unwrap()
            .into_inner();

        for i in 0..100 {
            service.deletion_tx.send(format!("sb-{i}")).unwrap();
        }

        let status = next_event(&mut stream)
            .await
            .expect_err("an overflowed receiver should report the gap");
        assert_eq!(status.code(), Code::DataLoss);
    }
}
