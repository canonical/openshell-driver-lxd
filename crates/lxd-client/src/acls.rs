// SPDX-License-Identifier: AGPL-3.0-or-later

//! Network ACL management.

use serde_json::json;

use crate::client::LxdClient;
use crate::error::LxdError;
use crate::types::Operation;

/// A single LXD Network ACL rule.
#[derive(Debug, Clone)]
pub struct LxdNetworkAclRule {
    /// Rule action.
    pub action: AclAction,
    /// Destination CIDR (e.g. `"10.0.0.0/8"`).
    pub destination: String,
    /// Destination port or range (e.g. `"8080"`, `"8080-8090"`).
    pub destination_port: String,
    /// IP protocol.
    pub protocol: AclProtocol,
    /// Rule state.
    pub state: AclState,
}

impl LxdNetworkAclRule {
    /// Allow egress TCP to a specific destination CIDR and port.
    pub fn allow_egress_tcp(dest_cidr: &str, dest_port: u16) -> Self {
        Self {
            action: AclAction::Allow,
            destination: dest_cidr.to_string(),
            destination_port: dest_port.to_string(),
            protocol: AclProtocol::Tcp,
            state: AclState::Enabled,
        }
    }
}

/// [`LxdNetworkAclRule::action`]: whether matching traffic is allowed or dropped.
#[derive(Copy, Clone, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum AclAction {
    Allow,
    Drop,
}

/// [`LxdNetworkAclRule::protocol`]: the IP protocol a rule matches on.
#[derive(Copy, Clone, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum AclProtocol {
    Tcp,
    Udp,
    Icmp,
}

/// [`LxdNetworkAclRule::state`]: whether a rule is enforced.
#[derive(Copy, Clone, Debug, Eq, PartialEq, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum AclState {
    Enabled,
    Disabled,
}

impl LxdClient {
    /// Ensures a named Network ACL exists with the given egress rules.
    ///
    /// Creates the ACL if it does not exist (`POST /1.0/network-acls`), or
    /// replaces the ruleset if it does (`PUT /1.0/network-acls/<name>`).
    /// Both endpoints run as background operations, so this waits on the
    /// resulting operation before returning; otherwise a caller that reads
    /// the ACL back immediately (or a subsequent call to this method) could
    /// race the operation and see stale state. Idempotent.
    pub async fn ensure_network_acl(
        &self,
        name: &str,
        egress: Vec<LxdNetworkAclRule>,
    ) -> Result<(), LxdError> {
        let egress_json: Vec<serde_json::Value> = egress
            .iter()
            .map(|r| {
                json!({
                    "action": r.action.to_string(),
                    "destination": r.destination,
                    "destination_port": r.destination_port,
                    "protocol": r.protocol.to_string(),
                    "state": r.state.to_string(),
                })
            })
            .collect();

        // LXD's PUT payload (NetworkACLPut) is the writable-fields-only shape;
        // unlike POST (NetworkACLsPost) it must not carry `name`, or LXD's
        // uniqueness validation on that field rejects the update as a
        // collision with the (identically-named) ACL it's replacing.
        let update_body = json!({
            "description": "OpenShell sandbox egress policy",
            "egress": egress_json,
            "ingress": [],
            "config": {},
        });
        let create_body = {
            let mut body = update_body.clone();
            body["name"] = json!(name);
            body
        };

        match self
            .get::<serde_json::Value>(&format!("/1.0/network-acls/{name}"))
            .await
        {
            Ok(_) => {
                let op = self
                    .put::<Operation>(&format!("/1.0/network-acls/{name}"), update_body)
                    .await?
                    .into_metadata()?;
                self.wait_operation(&op.id).await?;
            }
            Err(LxdError::Api {
                status_code: 404, ..
            }) => {
                match self
                    .post::<Operation>("/1.0/network-acls", create_body)
                    .await
                {
                    Ok(resp) => {
                        let op = resp.into_metadata()?;
                        self.wait_operation(&op.id).await?;
                    }
                    // A concurrent caller created the ACL between our GET and POST.
                    Err(LxdError::Api {
                        status_code: 409, ..
                    }) => {
                        let op = self
                            .put::<Operation>(&format!("/1.0/network-acls/{name}"), update_body)
                            .await?
                            .into_metadata()?;
                        self.wait_operation(&op.id).await?;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }

    /// Deletes a named Network ACL. A 404 response is treated as success.
    /// `DELETE /1.0/network-acls/<name>` runs as a background operation, so
    /// this waits on it before returning.
    pub async fn delete_network_acl(&self, name: &str) -> Result<(), LxdError> {
        match self
            .delete::<Operation>(&format!("/1.0/network-acls/{name}"))
            .await
        {
            Ok(resp) => {
                let op = resp.into_metadata()?;
                self.wait_operation(&op.id).await?;
                Ok(())
            }
            Err(LxdError::Api {
                status_code: 404, ..
            }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}
