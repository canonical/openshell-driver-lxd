// SPDX-License-Identifier: AGPL-3.0-or-later

//! LXD lifecycle event watcher.
//!
//! Without this, the gateway only learns that a sandbox died on its next
//! polling reconcile — up to a minute later. LXD publishes a lifecycle event
//! the moment an instance's init exits, so the driver subscribes to that
//! stream and pushes an updated sandbox snapshot straight away, the same way
//! the upstream Podman driver forwards runtime events.

use std::collections::HashMap;
use std::time::Duration;

use computev1::pb::DriverSandbox;
use lxd_client::LxdClient;
use tokio::sync::broadcast;

use crate::mapping;

/// How long to wait before re-subscribing after the event stream ends or
/// errors. LXD restarts (snap refreshes) drop the stream; reconnecting keeps
/// the driver reporting promptly afterwards without spinning on a daemon that
/// is still coming back up.
const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(2);

/// Lifecycle actions worth re-reading an instance for.
///
/// `instance-shutdown` is the interesting one: LXD emits it when the guest's
/// init exits by itself, which for a sandbox means the supervisor is gone.
/// `instance-stopped` is its counterpart for a stop issued through the API.
/// The rest keep the pushed snapshot honest across a sandbox's life;
/// `instance-created` also makes a new sandbox known, so its deletion can be
/// reported (see [`DELETED_ACTION`]).
const WATCHED_ACTIONS: &[&str] = &[
    "instance-created",
    "instance-started",
    "instance-shutdown",
    "instance-stopped",
    "instance-restarted",
    "instance-paused",
    "instance-resumed",
];

/// Lifecycle action for a deleted instance. There is nothing left to re-read,
/// so the watcher reports it as a deletion of the sandbox it last knew under
/// that name.
const DELETED_ACTION: &str = "instance-deleted";

/// Extracts the instance name from a lifecycle event's metadata.
///
/// Prefers the explicit `name` field and falls back to the last segment of
/// `source` (e.g. `/1.0/instances/my-sandbox`), which older LXD releases set
/// without a `name`. Outside the default project `source` carries the
/// project as a query (`/1.0/instances/my-sandbox?project=sandboxes`), which
/// is not part of the name.
fn instance_name(metadata: &serde_json::Value) -> Option<String> {
    if let Some(name) = metadata.get("name").and_then(|v| v.as_str()) {
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    metadata
        .get("source")
        .and_then(|v| v.as_str())
        .and_then(|source| source.split('?').next())
        .and_then(|path| path.rsplit('/').next())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// Extracts the lifecycle action (e.g. `instance-shutdown`).
fn action(metadata: &serde_json::Value) -> Option<&str> {
    metadata.get("action").and_then(|v| v.as_str())
}

/// Spawns the lifecycle watcher, which publishes a snapshot on `tx` whenever
/// a driver-managed instance changes state and a sandbox id on `deleted_tx`
/// when one is deleted, for `WatchSandboxes` to fan out to the gateway.
///
/// The task reconnects on its own and never terminates, so a subscription
/// failure at start-up (LXD not up yet) is not fatal.
pub(crate) fn spawn(
    lxd: LxdClient,
    tx: broadcast::Sender<DriverSandbox>,
    deleted_tx: broadcast::Sender<String>,
) {
    tokio::spawn(async move {
        loop {
            match run_once(&lxd, &tx, &deleted_tx).await {
                Ok(()) => {
                    tracing::debug!("LXD event stream ended; re-subscribing");
                }
                Err(e) => {
                    tracing::warn!(%e, "LXD event stream failed; re-subscribing");
                }
            }
            tokio::time::sleep(RESUBSCRIBE_DELAY).await;
        }
    });
}

/// Sandbox ids of the managed instances currently in the project, by name.
async fn managed_sandbox_ids(
    lxd: &LxdClient,
) -> Result<HashMap<String, String>, lxd_client::LxdError> {
    Ok(lxd
        .list_instances()
        .await?
        .into_iter()
        .filter_map(|instance| {
            let id = instance.config.get(mapping::KEY_SANDBOX_ID)?.clone();
            Some((instance.name, id))
        })
        .collect())
}

/// Subscribes once and forwards events until the stream ends.
async fn run_once(
    lxd: &LxdClient,
    tx: &broadcast::Sender<DriverSandbox>,
    deleted_tx: &broadcast::Sender<String>,
) -> Result<(), lxd_client::LxdError> {
    use futures::StreamExt;

    let mut stream = lxd.subscribe_events(&["lifecycle"]).await?;
    tracing::debug!("subscribed to LXD lifecycle events");

    // Which sandbox each instance name belongs to, so a deletion — after which
    // the instance can no longer be read — can still be reported by sandbox
    // id. Seeded after subscribing, so nothing created in between is missed.
    let mut sandbox_ids = managed_sandbox_ids(lxd).await?;

    while let Some(event) = stream.next().await {
        let event = event?;
        let Some(action) = action(&event.metadata) else {
            continue;
        };
        let Some(name) = instance_name(&event.metadata) else {
            continue;
        };

        if action == DELETED_ACTION {
            // Only sandboxes are reported. A delete through DeleteSandbox has
            // already published this; the gateway treats a repeat as a no-op.
            if let Some(sandbox_id) = sandbox_ids.remove(&name) {
                tracing::debug!(
                    name = %name,
                    sandbox_id = %sandbox_id,
                    "pushing sandbox deletion from lifecycle event"
                );
                deleted_tx.send(sandbox_id).ok();
            }
            continue;
        }

        if !WATCHED_ACTIONS.contains(&action) {
            continue;
        }

        // Re-read the instance rather than trusting the event: the event says
        // what happened, the instance says what state it left behind, and the
        // snapshot the gateway wants is built from the latter. A sandbox that
        // has since been deleted 404s here and is skipped; its deletion event
        // follows.
        let instance = match lxd.get_instance(&name).await {
            Ok(instance) => instance,
            Err(e) => {
                tracing::debug!(name = %name, %e, "could not read instance for lifecycle event");
                continue;
            }
        };

        // Only driver-managed instances are ours to report on; the event
        // stream carries every instance in the project.
        let Some(sandbox_id) = instance.config.get(mapping::KEY_SANDBOX_ID) else {
            continue;
        };
        sandbox_ids.insert(name.clone(), sandbox_id.clone());

        let sandbox = mapping::instance_to_driver_sandbox(&instance);
        tracing::debug!(
            name = %name,
            action = %action,
            status = %instance.status,
            "pushing sandbox snapshot from lifecycle event"
        );
        // An error here only means nothing is currently watching.
        tx.send(sandbox).ok();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_name_from_explicit_field() {
        let metadata = json!({"action": "instance-shutdown", "name": "sb-1"});
        assert_eq!(instance_name(&metadata).as_deref(), Some("sb-1"));
        assert_eq!(action(&metadata), Some("instance-shutdown"));
    }

    #[test]
    fn falls_back_to_source_path() {
        let metadata = json!({
            "action": "instance-stopped",
            "source": "/1.0/instances/sb-2",
        });
        assert_eq!(instance_name(&metadata).as_deref(), Some("sb-2"));
    }

    /// LXD appends the project to `source` outside the default project, e.g.
    /// `/1.0/instances/sb-3?project=sandboxes` (observed on LXD 6.9). The
    /// fallback is only taken when `name` is absent, which LXD 6.9 always
    /// sets.
    #[test]
    fn source_fallback_strips_project_query() {
        let metadata = json!({
            "action": "instance-shutdown",
            "source": "/1.0/instances/sb-3?project=sandboxes",
        });
        assert_eq!(instance_name(&metadata).as_deref(), Some("sb-3"));
    }

    #[test]
    fn explicit_name_wins_over_source() {
        let metadata = json!({
            "action": "instance-started",
            "name": "sb-4",
            "source": "/1.0/instances/sb-4?project=sandboxes",
        });
        assert_eq!(instance_name(&metadata).as_deref(), Some("sb-4"));
    }

    #[test]
    fn ignores_events_without_identity() {
        assert_eq!(instance_name(&json!({"action": "instance-shutdown"})), None);
        assert_eq!(instance_name(&json!({"name": ""})), None);
        assert_eq!(action(&json!({})), None);
    }

    #[test]
    fn watches_guest_death_and_api_stop() {
        // The pair that distinguishes "the supervisor exited" from "we were
        // asked to stop it" — the whole point of subscribing.
        assert!(WATCHED_ACTIONS.contains(&"instance-shutdown"));
        assert!(WATCHED_ACTIONS.contains(&"instance-stopped"));
        // Noise that should not trigger a re-read.
        assert!(!WATCHED_ACTIONS.contains(&"instance-log-retrieved"));
        assert!(!WATCHED_ACTIONS.contains(&"image-created"));
    }

    /// A deleted instance cannot be re-read; it is handled separately, by
    /// the sandbox id learned when it was created or first seen.
    #[test]
    fn deletion_is_not_a_re_read_action_but_creation_is() {
        assert!(!WATCHED_ACTIONS.contains(&DELETED_ACTION));
        assert!(WATCHED_ACTIONS.contains(&"instance-created"));
    }
}
