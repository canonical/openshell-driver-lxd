// SPDX-License-Identifier: AGPL-3.0-or-later

//! Core LXD compute driver logic, independent of the gRPC transport.

use computev1::pb::{DriverSandbox, GetCapabilitiesResponse};
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
    pub fn new(config: Config) -> Self {
        let lxd = LxdClient::new(config.lxd_socket.clone());
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

    /// MVP: always valid. Real validation (label-key format, resource
    /// quantity well-formedness) is deferred.
    pub async fn validate_sandbox_create(
        &self,
        _sandbox: &DriverSandbox,
    ) -> Result<(), DriverError> {
        Ok(())
    }

    pub async fn create_sandbox(&self, sandbox: &DriverSandbox) -> Result<(), DriverError> {
        let spec = sandbox
            .spec
            .as_ref()
            .ok_or_else(|| DriverError::InvalidArgument("sandbox.spec is required".to_string()))?;
        let template = spec.template.as_ref().ok_or_else(|| {
            DriverError::InvalidArgument("sandbox.spec.template is required".to_string())
        })?;

        let token = if spec.sandbox_token.is_empty() {
            None
        } else {
            Some(spec.sandbox_token.as_str())
        };

        self.provision_instance(sandbox, spec, template, token).await
    }

    async fn provision_instance(
        &self,
        sandbox: &DriverSandbox,
        spec: &computev1::pb::DriverSandboxSpec,
        template: &computev1::pb::DriverSandboxTemplate,
        token: Option<&str>,
    ) -> Result<(), DriverError> {
        let gateway_endpoint = self.resolve_gateway_endpoint(template).await?;

        let config =
            mapping::build_create_config(sandbox, spec, template, &gateway_endpoint, token.is_some())?;
        let has_gpu = spec
            .resource_requirements
            .as_ref()
            .and_then(|r| r.gpu.as_ref())
            .is_some();
        let devices = mapping::build_create_devices(template, has_gpu);
        let profiles = mapping::build_profiles(template);

        // Create stopped first so LXD fully applies the instance config
        // (security.nesting, ACL attachment, etc.) before the container starts.
        // Starting immediately via start=true races the kernel seccomp/cgroup
        // setup and the supervisor crashes on first boot.
        let op = self
            .lxd
            .create_instance(
                &sandbox.name,
                &self.config.default_image,
                config,
                devices,
                profiles,
                false,
            )
            .await?;
        self.lxd.wait_operation(&op.id, Some(60)).await?;

        // Push the sandbox JWT into the container's overlay filesystem before
        // starting. Writing directly via the LXD file API avoids the UID-mapping
        // permission issue that occurs when bind-mounting a host-user-owned file
        // into an unprivileged container (where container root ≠ host file owner).
        if let Some(jwt) = token {
            self.lxd
                .push_file_into_instance(
                    &sandbox.name,
                    mapping::GUEST_SANDBOX_TOKEN_PATH,
                    jwt.as_bytes(),
                )
                .await
                .map_err(|e| {
                    DriverError::Internal(format!(
                        "push token file into {}: {e}",
                        sandbox.name
                    ))
                })?;
        }

        let op = self.lxd.start_instance(&sandbox.name).await?;
        self.lxd.wait_operation(&op.id, Some(30)).await?;
        Ok(())
    }

    pub async fn get_sandbox(&self, name: &str) -> Result<DriverSandbox, DriverError> {
        let instance = self.lxd.get_instance(name).await?;
        let state = self.lxd.get_instance_state(name).await?;
        Ok(mapping::instance_to_driver_sandbox(&instance, &state))
    }

    pub async fn list_sandboxes(&self) -> Result<Vec<DriverSandbox>, DriverError> {
        let instances = self.lxd.list_instances().await?;
        let mut sandboxes = Vec::with_capacity(instances.len());
        for instance in instances {
            let state = self.lxd.get_instance_state(&instance.name).await?;
            sandboxes.push(mapping::instance_to_driver_sandbox(&instance, &state));
        }
        Ok(sandboxes)
    }

    pub async fn stop_sandbox(&self, name: &str) -> Result<(), DriverError> {
        let instance = match self.lxd.get_instance(name).await {
            Ok(instance) => instance,
            Err(LxdError::Api {
                status_code: 404, ..
            }) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        // LXD's stop endpoint errors (400, not 404) on an already-stopped
        // instance rather than treating it as a no-op, so check status
        // first instead of guessing at LXD's error text.
        if instance.status == "Stopped" {
            return Ok(());
        }

        let op = self.lxd.stop_instance(name, true).await?;
        self.lxd.wait_operation(&op.id, Some(30)).await?;
        Ok(())
    }

    /// Returns `true` only if an instance was actually removed: an unknown
    /// name is treated as a successful no-op (`deleted: false`), not an
    /// error, per `DeleteSandboxResponse.deleted`'s documented semantics.
    pub async fn delete_sandbox(&self, name: &str) -> Result<bool, DriverError> {
        let instance = match self.lxd.get_instance(name).await {
            Ok(instance) => instance,
            Err(LxdError::Api {
                status_code: 404, ..
            }) => return Ok(false),
            Err(err) => return Err(err.into()),
        };

        if instance.status != "Stopped" {
            let op = self.lxd.stop_instance(name, true).await?;
            self.lxd.wait_operation(&op.id, Some(30)).await?;
        }

        match self.lxd.delete_instance(name).await {
            Ok(op) => {
                self.lxd.wait_operation(&op.id, Some(30)).await?;
                Ok(true)
            }
            Err(LxdError::Api {
                status_code: 404, ..
            }) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// Resolves `OPENSHELL_ENDPOINT` from the sandbox's own target network's
    /// host-side bridge IP and the configured gateway gRPC port.
    async fn resolve_gateway_endpoint(
        &self,
        template: &computev1::pb::DriverSandboxTemplate,
    ) -> Result<String, DriverError> {
        let network_name = mapping::network(template);
        let network = self.lxd.get_network(network_name).await?;
        let cidr = network.config.get("ipv4.address").ok_or_else(|| {
            DriverError::InvalidArgument(format!(
                "network {network_name:?} has no ipv4.address configured"
            ))
        })?;
        let host_ip = cidr.split('/').next().unwrap_or(cidr);
        Ok(format!(
            "http://{host_ip}:{port}",
            port = self.config.gateway_grpc_port
        ))
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
        assert_eq!(response.default_image, "openshell-sandbox");
    }
}
