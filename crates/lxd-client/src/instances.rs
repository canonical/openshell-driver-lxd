// SPDX-License-Identifier: AGPL-3.0-or-later

//! Instance (container/VM) lifecycle methods.

use std::collections::HashMap;

use serde_json::json;

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
}
