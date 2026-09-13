// SPDX-License-Identifier: AGPL-3.0-or-later

//! Instance (container/VM) lifecycle methods.

use std::collections::HashMap;

use serde_json::json;
use urlencoding::encode;

use crate::client::LxdClient;
use crate::error::LxdError;
use crate::types::{Instance, InstanceState, Operation};

impl LxdClient {
    /// `POST /1.0/instances`: creates an instance from a local image alias.
    ///
    /// When `start` is `true`, LXD starts the instance as part of the same
    /// operation (`InstancesPost.start`), so no separate
    /// [`LxdClient::start_instance`] call is needed for the common
    /// create-and-start case.
    pub async fn create_instance(
        &self,
        name: &str,
        image_alias: &str,
        config: HashMap<String, String>,
        devices: HashMap<String, HashMap<String, String>>,
        profiles: Vec<String>,
        start: bool,
    ) -> Result<Operation, LxdError> {
        let body = json!({
            "name": name,
            "type": "container",
            "source": {
                "type": "image",
                "alias": image_alias,
            },
            "config": config,
            "devices": devices,
            "profiles": profiles,
            "start": start,
        });
        self.post::<Operation>("/1.0/instances", body)
            .await?
            .into_metadata()
    }

    /// `GET /1.0/instances/<name>`.
    pub async fn get_instance(&self, name: &str) -> Result<Instance, LxdError> {
        self.get::<Instance>(&format!("/1.0/instances/{name}"))
            .await?
            .into_metadata()
    }

    /// `GET /1.0/instances/<name>/state`.
    pub async fn get_instance_state(&self, name: &str) -> Result<InstanceState, LxdError> {
        self.get::<InstanceState>(&format!("/1.0/instances/{name}/state"))
            .await?
            .into_metadata()
    }

    /// `GET /1.0/instances?recursion=1`.
    pub async fn list_instances(&self) -> Result<Vec<Instance>, LxdError> {
        self.get::<Vec<Instance>>("/1.0/instances?recursion=1")
            .await?
            .into_metadata()
    }

    /// `PUT /1.0/instances/<name>/state` with `{action: "start"}`.
    pub async fn start_instance(&self, name: &str) -> Result<Operation, LxdError> {
        let body = json!({"action": "start"});
        self.put::<Operation>(&format!("/1.0/instances/{name}/state"), body)
            .await?
            .into_metadata()
    }

    /// `PUT /1.0/instances/<name>/state` with `{action: "stop", force}`.
    pub async fn stop_instance(&self, name: &str, force: bool) -> Result<Operation, LxdError> {
        let body = json!({"action": "stop", "force": force});
        self.put::<Operation>(&format!("/1.0/instances/{name}/state"), body)
            .await?
            .into_metadata()
    }

    /// `DELETE /1.0/instances/<name>`.
    pub async fn delete_instance(&self, name: &str) -> Result<Operation, LxdError> {
        self.delete::<Operation>(&format!("/1.0/instances/{name}"))
            .await?
            .into_metadata()
    }

    /// `POST /1.0/instances/<name>/files?path=<guest_path>`: write a file
    /// directly into the container's overlay filesystem.
    ///
    /// The container does not need to be running — LXD accesses the overlay
    /// directly for containers (not VMs). The file is created with
    /// `uid=0 gid=0 mode=0400` inside the container (owned by container root,
    /// read-only). This sidesteps the UID-mapping problem that arises when
    /// bind-mounting a host file: a file owned by the host user (e.g. UID 1000)
    /// appears inside an unprivileged container as the overflow UID (65534), which
    /// container root cannot read.
    pub async fn push_file_into_instance(
        &self,
        name: &str,
        guest_path: &str,
        content: &[u8],
    ) -> Result<(), LxdError> {
        // LXD's file-push API does not create missing parent directories, so
        // create each ancestor first. The old purpose-built sandbox image
        // shipped the token directory as a placeholder; with arbitrary base
        // images (e.g. the upstream supervisor image) it may not exist.
        self.create_parent_dirs_in_instance(name, guest_path)
            .await?;

        let encoded_path = encode(guest_path);
        self.post_raw(
            &format!("/1.0/instances/{name}/files?path={encoded_path}"),
            "application/octet-stream",
            &[
                ("X-LXD-uid", "0"),
                ("X-LXD-gid", "0"),
                ("X-LXD-mode", "0400"),
                ("X-LXD-type", "file"),
                ("X-LXD-write", "overwrite"),
            ],
            hyper::body::Bytes::copy_from_slice(content),
        )
        .await
    }

    /// Creates every ancestor directory of `guest_path` inside the container,
    /// shallowest first, tolerating directories that already exist. Uses the
    /// LXD files API with `X-LXD-type: directory`; the container need not be
    /// running (same overlay-access rules as file push).
    async fn create_parent_dirs_in_instance(
        &self,
        name: &str,
        guest_path: &str,
    ) -> Result<(), LxdError> {
        let mut prefix = String::new();
        let components: Vec<&str> = guest_path.split('/').filter(|c| !c.is_empty()).collect();
        // Skip the last component: it is the file itself, not a directory.
        for component in components.iter().take(components.len().saturating_sub(1)) {
            prefix.push('/');
            prefix.push_str(component);
            let encoded_path = encode(&prefix);
            let result = self
                .post_raw(
                    &format!("/1.0/instances/{name}/files?path={encoded_path}"),
                    "application/octet-stream",
                    &[
                        ("X-LXD-uid", "0"),
                        ("X-LXD-gid", "0"),
                        ("X-LXD-mode", "0755"),
                        ("X-LXD-type", "directory"),
                    ],
                    hyper::body::Bytes::new(),
                )
                .await;
            match result {
                Ok(()) => {}
                // A directory that already exists is fine; LXD reports it with
                // a 500/"already exists" style error. Anything else propagates.
                Err(LxdError::Api { message, .. }) if message.contains("exists") => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// `GET /1.0/instances/<name>/files?path=<guest_path>`: fetches a file from
    /// an instance, returning its content and mode (e.g. `0o755`).
    pub async fn get_file_from_instance(
        &self,
        name: &str,
        guest_path: &str,
    ) -> Result<(hyper::body::Bytes, u32), LxdError> {
        let encoded_path = encode(guest_path);
        let path = format!("/1.0/instances/{name}/files?path={encoded_path}");
        let (headers, body) = self.get_raw_with_headers(&path).await?;
        let mode_str = headers
            .get("X-LXD-mode")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("0644");
        let clean_mode = mode_str.trim_start_matches('0');
        let mode = if clean_mode.is_empty() {
            0
        } else {
            u32::from_str_radix(clean_mode, 8).unwrap_or(0)
        };
        Ok((body, mode))
    }
}
