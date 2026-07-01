// SPDX-License-Identifier: AGPL-3.0-or-later

//! Network ACL management.

use serde_json::json;

use crate::client::LxdClient;
use crate::error::LxdError;

/// A single LXD Network ACL rule.
#[derive(Debug, Clone)]
pub struct LxdNetworkAclRule {
    pub action: String,
    pub destination: String,
    pub destination_port: String,
    pub protocol: String,
    pub state: String,
}

impl LxdNetworkAclRule {
    /// Allow egress TCP to a specific host CIDR + port.
    pub fn allow_egress_tcp(dest_cidr: &str, dest_port: u16) -> Self {
        Self {
            action: "allow".to_string(),
            destination: dest_cidr.to_string(),
            destination_port: dest_port.to_string(),
            protocol: "tcp".to_string(),
            state: "enabled".to_string(),
        }
    }
}

impl LxdClient {
    /// Ensures a named Network ACL exists with the given egress rules.
    ///
    /// If the ACL does not exist it is created (`POST /1.0/network-acls`).
    /// If it already exists it is replaced with the new ruleset
    /// (`PUT /1.0/network-acls/<name>`). Idempotent.
    pub async fn ensure_network_acl(
        &self,
        name: &str,
        egress: Vec<LxdNetworkAclRule>,
    ) -> Result<(), LxdError> {
        let egress_json: Vec<serde_json::Value> = egress
            .iter()
            .map(|r| {
                json!({
                    "action": r.action,
                    "destination": r.destination,
                    "destination_port": r.destination_port,
                    "protocol": r.protocol,
                    "state": r.state,
                })
            })
            .collect();

        let body = json!({
            "name": name,
            "description": "OpenShell sandbox egress policy",
            "egress": egress_json,
            "ingress": [],
            "config": {},
        });

        match self
            .get::<serde_json::Value>(&format!("/1.0/network-acls/{name}"))
            .await
        {
            Ok(_) => {
                self.put::<serde_json::Value>(&format!("/1.0/network-acls/{name}"), body)
                    .await?;
            }
            Err(LxdError::Api {
                status_code: 404, ..
            }) => {
                self.post::<serde_json::Value>("/1.0/network-acls", body)
                    .await?;
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Deletes a named Network ACL. A 404 response is treated as success.
    pub async fn delete_network_acl(&self, name: &str) -> Result<(), LxdError> {
        match self
            .delete::<serde_json::Value>(&format!("/1.0/network-acls/{name}"))
            .await
        {
            Ok(_)
            | Err(LxdError::Api {
                status_code: 404, ..
            }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}
