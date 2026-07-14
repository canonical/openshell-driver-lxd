// SPDX-License-Identifier: AGPL-3.0-or-later

//! Core LXD compute driver logic, independent of the gRPC transport.

use std::time::Duration;

use computev1::pb::{DriverSandbox, DriverSandboxTemplate, GetCapabilitiesResponse};
use lxd_client::{LxdClient, LxdError};

use crate::config::Config;
use crate::error::DriverError;
use crate::mapping;

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
        let devices = mapping::build_create_devices(template, gpu.is_some());
        let profiles = mapping::build_profiles(template);

        let image = if template.image.is_empty() {
            &self.config.default_image
        } else {
            &template.image
        };

        // Create the instance stopped so we can push the token file before the
        // supervisor starts — avoids a race where the supervisor reads
        // OPENSHELL_SANDBOX_TOKEN_FILE before it has been written.
        let op = self
            .lxd
            .create_instance(&sandbox.name, image, config, devices, profiles, false)
            .await?;
        self.wait_operation(&op.id).await?;

        if has_token {
            self.lxd
                .push_file_into_instance(
                    &sandbox.name,
                    mapping::GUEST_SANDBOX_TOKEN_PATH,
                    spec.sandbox_token.as_bytes(),
                )
                .await?;
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
        assert_eq!(response.default_image, "openshell-sandbox");
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
}
