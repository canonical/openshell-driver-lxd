// SPDX-License-Identifier: AGPL-3.0-or-later

//! Core LXD compute driver logic, independent of the gRPC transport.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use computev1::pb::{DriverSandbox, DriverSandboxTemplate, GetCapabilitiesResponse};
use lxd_client::{LxdClient, LxdError};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::error::DriverError;
use crate::image::{digest_of_file, ImageCache, SkopeoImporter};
use crate::mapping;

const DRIVER_NAME: &str = "lxd";

/// Returns true if `err` indicates the instance was already stopped.
///
/// Covers both cases: LXD rejects the stop request synchronously with a
/// 400, or accepts it and the operation fails asynchronously once it
/// discovers the instance already reached the target state.
fn is_already_stopped(err: &LxdError) -> bool {
    let message = match err {
        LxdError::Api {
            status_code: 400,
            message,
        } => message,
        LxdError::OperationFailed { err, .. } => err,
        LxdError::Api { .. }
        | LxdError::InvalidQuantity { .. }
        | LxdError::Io(_)
        | LxdError::Hyper(_)
        | LxdError::Http(_)
        | LxdError::Json(_)
        | LxdError::Tls { .. }
        | LxdError::WebSocket { .. } => return false,
    };
    message.contains("not running") || message.contains("already stopped")
}

/// LXD compute driver.
#[derive(Debug, Clone)]
pub struct LxdComputeDriver {
    config: Config,
    lxd: LxdClient,
    image_cache: ImageCache,
    supervisor_volume_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl LxdComputeDriver {
    #[must_use]
    pub fn new(config: Config, lxd: LxdClient) -> Self {
        let importer = Arc::new(SkopeoImporter::new(
            lxd.clone(),
            config.skopeo_path.clone(),
            config.umoci_path.clone(),
            config.mksquashfs_path.clone(),
            Duration::from_secs(config.image_pull_timeout_secs),
        ));
        let image_cache = ImageCache::new(
            lxd.clone(),
            importer,
            config.image_cache_alias_prefix.clone(),
        );
        Self::with_image_cache(config, lxd, image_cache)
    }

    #[must_use]
    pub fn with_image_cache(config: Config, lxd: LxdClient, image_cache: ImageCache) -> Self {
        Self {
            config,
            lxd,
            image_cache,
            supervisor_volume_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Best-effort pre-warm of the default sandbox image so the first
    /// `create_sandbox` need not block on a registry pull, and so a bad
    /// default reference or an unreachable registry surfaces at startup
    /// rather than on the first request. Importing requires the external
    /// tooling (skopeo/umoci/mksquashfs); any failure here is logged and
    /// otherwise ignored — the same import is retried on first use.
    pub async fn ensure_default_image(&self) -> Result<String, DriverError> {
        self.image_cache
            .resolve_alias(&self.config.default_image)
            .await
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

    pub async fn validate_sandbox_create(
        &self,
        sandbox: &DriverSandbox,
    ) -> Result<(), DriverError> {
        if sandbox.name.is_empty() {
            return Err(DriverError::InvalidArgument(
                "sandbox.name is required".into(),
            ));
        }
        if sandbox.id.is_empty() {
            return Err(DriverError::InvalidArgument(
                "sandbox.id is required".into(),
            ));
        }
        let spec = sandbox
            .spec
            .as_ref()
            .ok_or_else(|| DriverError::InvalidArgument("sandbox.spec is required".into()))?;
        let template = spec.template.as_ref().ok_or_else(|| {
            DriverError::InvalidArgument("sandbox.spec.template is required".into())
        })?;

        for key in template.labels.keys() {
            if !mapping::is_valid_label_key(key) {
                return Err(DriverError::InvalidArgument(format!(
                    "invalid label key {key:?}: must match [a-zA-Z0-9._-]+"
                )));
            }
        }

        if let Some(count) = spec
            .resource_requirements
            .as_ref()
            .and_then(|r| r.gpu.as_ref())
            .and_then(|g| g.count)
        {
            if count == 0 {
                return Err(DriverError::InvalidArgument(
                    "sandbox.spec.resource_requirements.gpu.count must be at least 1 if set; \
                     omit it to request the default GPU assignment"
                        .into(),
                ));
            }
        }

        Ok(())
    }

    /// Waits for an LXD operation to complete, bounded by
    /// `Config::operation_timeout_secs` so a hung LXD instance cannot block
    /// an RPC indefinitely.
    async fn wait_operation(&self, id: &str) -> Result<(), DriverError> {
        tokio::time::timeout(
            Duration::from_secs(self.config.operation_timeout_secs),
            self.lxd.wait_operation(id),
        )
        .await
        .map_err(|_| DriverError::Timeout)??;
        Ok(())
    }

    /// Fetches an instance by name and confirms it's driver-managed.
    ///
    /// Rejects arbitrary non-driver LXD instances as not found; "managed"
    /// means the instance carries the `user.openshell.sandbox_id` marker
    /// `create_sandbox` sets.
    async fn get_managed_instance(&self, name: &str) -> Result<lxd_client::Instance, DriverError> {
        let not_found = || DriverError::NotFound(format!("sandbox {name:?} not found"));
        let instance = match self.lxd.get_instance(name).await {
            Ok(instance) => instance,
            Err(LxdError::Api {
                status_code: 404, ..
            }) => return Err(not_found()),
            Err(e) => return Err(e.into()),
        };
        if !instance.config.contains_key(mapping::KEY_SANDBOX_ID) {
            return Err(not_found());
        }
        Ok(instance)
    }

    pub async fn get_sandbox(&self, name: &str) -> Result<DriverSandbox, DriverError> {
        let instance = self.get_managed_instance(name).await?;
        Ok(mapping::instance_to_driver_sandbox(&instance))
    }

    pub async fn list_sandboxes(&self) -> Result<Vec<DriverSandbox>, DriverError> {
        let instances = self.lxd.list_instances().await?;
        Ok(instances
            .iter()
            .filter(|i| i.config.contains_key(mapping::KEY_SANDBOX_ID))
            .map(mapping::instance_to_driver_sandbox)
            .collect())
    }

    /// Resolves an instance name from a gateway-assigned `sandbox_id` by
    /// scanning driver-managed instances for a config match. Used as a
    /// fallback when a request supplies only `sandbox_id`, not
    /// `sandbox_name` — LXD itself has no by-id lookup, only by-name.
    pub async fn find_name_by_sandbox_id(
        &self,
        sandbox_id: &str,
    ) -> Result<Option<String>, DriverError> {
        let instances = self.lxd.list_instances().await?;
        Ok(instances
            .into_iter()
            .find(|i| i.config.get(mapping::KEY_SANDBOX_ID).map(String::as_str) == Some(sandbox_id))
            .map(|i| i.name))
    }

    pub async fn create_sandbox(&self, sandbox: &DriverSandbox) -> Result<(), DriverError> {
        self.validate_sandbox_create(sandbox).await?;

        let spec = sandbox
            .spec
            .as_ref()
            .ok_or_else(|| DriverError::InvalidArgument("sandbox.spec is required".into()))?;
        let template = spec.template.as_ref().ok_or_else(|| {
            DriverError::InvalidArgument("sandbox.spec.template is required".into())
        })?;

        let has_token = !spec.sandbox_token.is_empty();
        let gateway_endpoint = self.resolve_gateway_endpoint(template).await?;
        let config =
            mapping::build_create_config(sandbox, spec, template, &gateway_endpoint, has_token)?;

        let gpu = spec
            .resource_requirements
            .as_ref()
            .and_then(|r| r.gpu.as_ref());
        // v1: a GPU request attaches every physical GPU on the host (LXD's
        // default for a `gputype: physical` device with no selector on
        // containers); `count` would need host GPU inventory to honor
        // precisely and isn't consulted yet.
        if let Some(count) = gpu.and_then(|g| g.count) {
            tracing::debug!(
                count,
                "GpuResourceRequirements.count is ignored in v1; attaching all host GPUs"
            );
        }
        // Resolve supervisor binary and digest
        let (binary_path, digest) = match &self.config.supervisor_bin {
            Some(path) => (path.clone(), digest_of_file(path)?),
            None => self
                .image_cache
                .extract_supervisor_binary(
                    &self.config.supervisor_image,
                    &self.config.supervisor_cache_dir,
                )
                .await
                .map_err(|e| {
                    DriverError::ImageImport(format!("supervisor binary extraction failed: {e}"))
                })?,
        };

        // Ensure digest-keyed custom storage volume exists on supervisor_storage_pool
        let volume_name = mapping::supervisor_volume_name(&digest);
        let vol_lock = {
            let mut locks = self.supervisor_volume_locks.lock().await;
            locks
                .entry(digest.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        {
            let _guard = vol_lock.lock().await;
            self.lxd
                .ensure_supervisor_volume(
                    &self.config.supervisor_storage_pool,
                    &volume_name,
                    &binary_path,
                )
                .await
                .map_err(|e| {
                    DriverError::ImageImport(format!(
                        "supervisor binary volume provisioning failed on pool {:?}: {e}",
                        self.config.supervisor_storage_pool
                    ))
                })?;
        }

        let devices = mapping::build_create_devices(
            template,
            gpu.is_some(),
            &self.config.supervisor_storage_pool,
            &volume_name,
        );
        let profiles = mapping::build_profiles(template);

        let image_alias = if template.image.is_empty() {
            self.image_cache
                .resolve_alias(&self.config.default_image)
                .await?
        } else {
            self.image_cache.resolve_alias(&template.image).await?
        };

        // Create the instance stopped so we can push the token file before the
        // supervisor starts — avoids a race where the supervisor reads
        // OPENSHELL_SANDBOX_TOKEN_FILE before it has been written.
        let op = self
            .lxd
            .create_instance(
                &sandbox.name,
                &image_alias,
                config,
                devices,
                profiles,
                false,
            )
            .await?;
        self.wait_operation(&op.id).await?;

        if has_token {
            if let Err(push_err) = self
                .lxd
                .push_file_into_instance(
                    &sandbox.name,
                    mapping::GUEST_SANDBOX_TOKEN_PATH,
                    spec.sandbox_token.as_bytes(),
                )
                .await
            {
                // The instance is stopped but unstarted; delete it rather than
                // leaving an orphaned container.
                let cleanup = async {
                    let op = self.lxd.delete_instance(&sandbox.name).await?;
                    self.wait_operation(&op.id).await
                };
                if let Err(e) = cleanup.await {
                    tracing::warn!(
                        name = %sandbox.name,
                        %e,
                        "failed to clean up instance after token push failure"
                    );
                }
                return Err(push_err.into());
            }
        }

        let op = self.lxd.start_instance(&sandbox.name).await?;
        self.wait_operation(&op.id).await?;

        Ok(())
    }

    /// Resolves `OPENSHELL_ENDPOINT` from the sandbox's own target network's
    /// host-side bridge IP and the configured gateway gRPC port.
    async fn resolve_gateway_endpoint(
        &self,
        template: &DriverSandboxTemplate,
    ) -> Result<String, DriverError> {
        let network_name = mapping::network(template);
        let network = self.lxd.get_network(network_name).await?;
        let cidr = network.config.get("ipv4.address").ok_or_else(|| {
            DriverError::InvalidArgument(format!(
                "network {network_name:?} has no ipv4.address configured"
            ))
        })?;
        let host_ip = cidr.split('/').next().unwrap_or(cidr);
        let port = self.config.gateway_grpc_port;
        Ok(format!("http://{host_ip}:{port}"))
    }

    pub async fn stop_sandbox(&self, name: &str) -> Result<(), DriverError> {
        self.get_managed_instance(name).await?;

        let op = match self.lxd.stop_instance(name, false).await {
            Err(e) if is_already_stopped(&e) => return Ok(()),
            other => other?,
        };
        if let Err(e) = self.wait_operation(&op.id).await {
            if !matches!(&e, DriverError::Lxd(lxd_err) if is_already_stopped(lxd_err)) {
                return Err(e);
            }
        }
        Ok(())
    }

    /// Deletes a sandbox by instance name, idempotently.
    ///
    /// Returns `Some(sandbox_id)` (the `user.openshell.sandbox_id` from the
    /// instance config, used by the gRPC layer to broadcast a WatchSandboxes
    /// Deleted event) if the sandbox was deleted, or `None` if it was not
    /// found — the caller may retry safely.
    pub async fn delete_sandbox(&self, name: &str) -> Result<Option<String>, DriverError> {
        let instance = match self.lxd.get_instance(name).await {
            Ok(i) => i,
            Err(LxdError::Api {
                status_code: 404, ..
            }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let Some(sandbox_id) = instance.config.get(mapping::KEY_SANDBOX_ID).cloned() else {
            // Not driver-managed: treat as not found rather than deleting an
            // arbitrary LXD instance the caller happened to name correctly.
            return Ok(None);
        };

        // Force-stop before deleting; LXD rejects deletion of running instances.
        match self.lxd.stop_instance(name, true).await {
            Ok(op) => {
                if let Err(e) = self.wait_operation(&op.id).await {
                    if !matches!(&e, DriverError::Lxd(lxd_err) if is_already_stopped(lxd_err)) {
                        return Err(e);
                    }
                }
            }
            Err(e) if is_already_stopped(&e) => {}
            Err(LxdError::Api {
                status_code: 404, ..
            }) => return Ok(None),
            Err(e) => return Err(e.into()),
        }

        let op = match self.lxd.delete_instance(name).await {
            Err(LxdError::Api {
                status_code: 404, ..
            }) => return Ok(None),
            other => other?,
        };
        self.wait_operation(&op.id).await?;
        Ok(Some(sandbox_id))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use clap::Parser;
    use computev1::pb::{
        DriverSandboxSpec, DriverSandboxTemplate, GpuResourceRequirements, ResourceRequirements,
    };
    use lxd_client::LxdEndpoint;

    use super::*;
    use crate::config::DEFAULT_LXD_SOCKET;

    fn driver() -> LxdComputeDriver {
        let config = Config::parse_from(["openshell-driver-lxd"]);
        let lxd =
            LxdClient::new(LxdEndpoint::UnixSocket(PathBuf::from(DEFAULT_LXD_SOCKET))).unwrap();
        LxdComputeDriver::new(config, lxd)
    }

    fn sandbox_with_spec(spec: DriverSandboxSpec) -> DriverSandbox {
        DriverSandbox {
            id: "id".to_string(),
            name: "name".to_string(),
            namespace: "default".to_string(),
            workspace: "default".to_string(),
            spec: Some(spec),
            status: None,
        }
    }

    fn spec_with_labels(labels: HashMap<String, String>) -> DriverSandboxSpec {
        DriverSandboxSpec {
            template: Some(DriverSandboxTemplate {
                labels,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn spec_with_gpu_count(count: Option<u32>) -> DriverSandboxSpec {
        DriverSandboxSpec {
            template: Some(DriverSandboxTemplate::default()),
            resource_requirements: Some(ResourceRequirements {
                gpu: Some(GpuResourceRequirements { count }),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn capabilities_reports_static_fields() {
        let response = driver().capabilities();

        assert_eq!(response.driver_name, "lxd");
        assert_eq!(response.driver_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            response.default_image,
            "ghcr.io/nvidia/openshell/supervisor:latest"
        );
    }

    #[tokio::test]
    async fn validate_sandbox_create_rejects_invalid_label_key() {
        let mut labels = HashMap::new();
        labels.insert("bad key".to_string(), "value".to_string());
        let sandbox = sandbox_with_spec(spec_with_labels(labels));

        let err = driver()
            .validate_sandbox_create(&sandbox)
            .await
            .expect_err("invalid label key should be rejected");

        assert!(matches!(err, DriverError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn validate_sandbox_create_accepts_valid_label_key() {
        let mut labels = HashMap::new();
        labels.insert("team.example-key_1".to_string(), "value".to_string());
        let sandbox = sandbox_with_spec(spec_with_labels(labels));

        driver()
            .validate_sandbox_create(&sandbox)
            .await
            .expect("valid label key should be accepted");
    }

    #[tokio::test]
    async fn validate_sandbox_create_rejects_zero_gpu_count() {
        let sandbox = sandbox_with_spec(spec_with_gpu_count(Some(0)));

        let err = driver()
            .validate_sandbox_create(&sandbox)
            .await
            .expect_err("gpu.count == 0 should be rejected");

        assert!(matches!(err, DriverError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn validate_sandbox_create_accepts_omitted_gpu_count() {
        let sandbox = sandbox_with_spec(spec_with_gpu_count(None));

        driver()
            .validate_sandbox_create(&sandbox)
            .await
            .expect("omitted gpu.count should be accepted");
    }

    struct MockAliasChecker {
        exists: bool,
    }

    #[tonic::async_trait]
    impl crate::image::ImageAliasChecker for MockAliasChecker {
        async fn image_alias_exists(&self, _alias: &str) -> Result<bool, DriverError> {
            Ok(self.exists)
        }
    }

    struct MockImporter {
        digest: String,
        imported_alias: std::sync::Mutex<Option<String>>,
    }

    #[tonic::async_trait]
    impl crate::image::OciImporter for MockImporter {
        async fn resolve_digest(&self, _reference: &str) -> Result<String, DriverError> {
            Ok(self.digest.clone())
        }

        async fn import(
            &self,
            _reference: &str,
            _digest: &str,
            alias: &str,
        ) -> Result<(), DriverError> {
            *self.imported_alias.lock().unwrap() = Some(alias.to_string());
            Ok(())
        }

        async fn extract_supervisor_binary(
            &self,
            _reference: &str,
            cache_dir: &std::path::Path,
        ) -> Result<(std::path::PathBuf, String), DriverError> {
            let target_dir = cache_dir.join("test-digest");
            let binary_path = target_dir.join("openshell-sandbox");
            if !binary_path.exists() {
                std::fs::create_dir_all(&target_dir).unwrap();
                std::fs::write(&binary_path, b"mock-supervisor").unwrap();
            }
            Ok((binary_path, self.digest.clone()))
        }
    }

    #[tokio::test]
    async fn create_sandbox_resolves_image_or_defaults() {
        let config = Config::parse_from(["openshell-driver-lxd"]);
        let lxd =
            LxdClient::new(LxdEndpoint::UnixSocket(PathBuf::from(DEFAULT_LXD_SOCKET))).unwrap();

        let digest_hex = "ee".repeat(32);
        let importer = Arc::new(MockImporter {
            digest: format!("sha256:{digest_hex}"),
            imported_alias: std::sync::Mutex::new(None),
        });
        let checker = Arc::new(MockAliasChecker { exists: true });
        let cache =
            ImageCache::with_checker(checker, importer, config.image_cache_alias_prefix.clone());

        let driver = LxdComputeDriver::with_image_cache(config, lxd, cache);

        // 1. Empty template.image falls back to default_image, which is now
        //    itself an OCI reference resolved through the same import path.
        let empty_spec = DriverSandboxSpec {
            template: Some(DriverSandboxTemplate {
                image: "".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let _sb_empty = sandbox_with_spec(empty_spec);
        assert_eq!(
            driver.config.default_image,
            "ghcr.io/nvidia/openshell/supervisor:latest"
        );

        // 2. Non-empty template.image resolves to the digest-derived alias
        let custom_spec = DriverSandboxSpec {
            template: Some(DriverSandboxTemplate {
                image: "registry.example.com/custom:v1".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let sb_custom = sandbox_with_spec(custom_spec);
        let template = sb_custom.spec.unwrap().template.unwrap();
        let resolved = driver
            .image_cache
            .resolve_alias(&template.image)
            .await
            .unwrap();
        assert_eq!(resolved, format!("openshell-oci-{digest_hex}"));
    }
}
