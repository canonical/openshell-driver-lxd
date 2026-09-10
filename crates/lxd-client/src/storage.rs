// SPDX-License-Identifier: AGPL-3.0-or-later

//! Custom storage volume management on LXD storage pools.

use std::path::Path;

use hyper::body::Bytes;
use serde_json::json;
use urlencoding::encode;

use crate::client::LxdClient;
use crate::error::LxdError;
use crate::types::Operation;

impl LxdClient {
    /// Checks whether a custom storage pool volume exists on the given pool.
    pub async fn storage_pool_volume_exists(
        &self,
        pool: &str,
        volume_type: &str,
        name: &str,
    ) -> Result<bool, LxdError> {
        let path = format!(
            "/1.0/storage-pools/{}/volumes/{}/{}",
            encode(pool),
            encode(volume_type),
            encode(name)
        );
        match self.get::<serde_json::Value>(&path).await {
            Ok(_) => Ok(true),
            Err(LxdError::Api {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Creates a custom storage volume from a tarball payload on the given storage pool.
    ///
    /// Sends a `POST /1.0/storage-pools/<pool>/volumes` with the raw tarball
    /// and `X-LXD-name: <name>`.
    pub async fn create_storage_pool_volume_from_tarball(
        &self,
        pool: &str,
        name: &str,
        tarball_bytes: &[u8],
    ) -> Result<Operation, LxdError> {
        let path = format!("/1.0/storage-pools/{}/volumes", encode(pool));
        let response = self
            .post_raw_response::<Operation>(
                &path,
                "application/octet-stream",
                &[("X-LXD-name", name), ("X-LXD-type", "tar")],
                Bytes::copy_from_slice(tarball_bytes),
            )
            .await?;
        response.into_metadata()
    }

    /// Updates custom storage pool volume configuration (e.g. setting `security.shifted`).
    pub async fn update_storage_pool_volume_config(
        &self,
        pool: &str,
        volume_type: &str,
        name: &str,
        config: serde_json::Value,
    ) -> Result<Operation, LxdError> {
        let path = format!(
            "/1.0/storage-pools/{}/volumes/{}/{}",
            encode(pool),
            encode(volume_type),
            encode(name)
        );
        let body = json!({
            "config": config,
        });
        let response = self.put::<Operation>(&path, body).await?;
        response.into_metadata()
    }

    /// Deletes a custom storage pool volume.
    pub async fn delete_storage_pool_volume(
        &self,
        pool: &str,
        volume_type: &str,
        name: &str,
    ) -> Result<Operation, LxdError> {
        let path = format!(
            "/1.0/storage-pools/{}/volumes/{}/{}",
            encode(pool),
            encode(volume_type),
            encode(name)
        );
        let response = self.delete::<Operation>(&path).await?;
        response.into_metadata()
    }

    /// Ensures a digest-keyed supervisor storage volume exists on `pool`.
    ///
    /// Packages `binary` into a single-entry tarball containing `openshell-sandbox`,
    /// creates the volume with `content-type: filesystem` and `security.shifted: true`,
    /// and treats an "already exists" conflict as success.
    pub async fn ensure_supervisor_volume(
        &self,
        pool: &str,
        name: &str,
        binary: &Path,
    ) -> Result<(), LxdError> {
        if self
            .storage_pool_volume_exists(pool, "custom", name)
            .await?
        {
            tracing::debug!(pool = %pool, name = %name, "supervisor volume already exists");
            return Ok(());
        }

        let binary_bytes = tokio::fs::read(binary).await?;
        let tarball_bytes = create_single_file_tarball("openshell-sandbox", &binary_bytes, 0o755)?;

        match self
            .create_storage_pool_volume_from_tarball(pool, name, &tarball_bytes)
            .await
        {
            Ok(op) => match self.wait_operation(&op.id).await {
                Ok(_) => {}
                Err(e) if is_volume_already_exists_error(&e) => {
                    tracing::debug!(
                        pool = %pool,
                        name = %name,
                        "volume creation raced; volume already exists"
                    );
                    return Ok(());
                }
                Err(e) => return Err(e),
            },
            Err(e) if is_volume_already_exists_error(&e) => {
                tracing::debug!(
                    pool = %pool,
                    name = %name,
                    "volume creation raced (synchronous conflict); volume already exists"
                );
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        // Configure security.shifted: true so unprivileged containers can access the files
        let config = json!({
            "security.shifted": "true",
        });
        if let Ok(op) = self
            .update_storage_pool_volume_config(pool, "custom", name, config)
            .await
        {
            let _ = self.wait_operation(&op.id).await;
        }

        Ok(())
    }
}

/// Helper to create an in-memory tarball containing a single file at root.
pub(crate) fn create_single_file_tarball(
    filename: &str,
    contents: &[u8],
    mode: u32,
) -> Result<Vec<u8>, std::io::Error> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(mode);
    header.set_cksum();
    builder.append_data(&mut header, filename, contents)?;
    builder.into_inner()
}

/// Checks whether an error from volume creation represents an already-exists conflict.
pub(crate) fn is_volume_already_exists_error(err: &LxdError) -> bool {
    match err {
        LxdError::Api {
            status_code: 409, ..
        } => true,
        LxdError::Api { message, .. } => {
            let lower = message.to_lowercase();
            lower.contains("already exists") || lower.contains("unique")
        }
        LxdError::OperationFailed { err, .. } => {
            let lower = err.to_lowercase();
            lower.contains("already exists") || lower.contains("unique")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_single_file_tarball() {
        let content = b"echo hello world";
        let tar_bytes = create_single_file_tarball("openshell-sandbox", content, 0o755).unwrap();

        let mut archive = tar::Archive::new(&tar_bytes[..]);
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().to_str().unwrap(), "openshell-sandbox");
        assert_eq!(entry.header().mode().unwrap(), 0o755);

        let mut extracted = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut extracted).unwrap();
        assert_eq!(extracted, content);
        assert!(entries.next().is_none());
    }

    #[test]
    fn test_is_volume_already_exists_error() {
        assert!(is_volume_already_exists_error(&LxdError::Api {
            status_code: 409,
            message: "Conflict".into(),
        }));
        assert!(is_volume_already_exists_error(&LxdError::Api {
            status_code: 400,
            message: "volume already exists on storage pool".into(),
        }));
        assert!(is_volume_already_exists_error(&LxdError::OperationFailed {
            description: "Creating storage volume".into(),
            err: "UNIQUE constraint failed: storage_volumes...".into(),
        }));
        assert!(!is_volume_already_exists_error(&LxdError::Api {
            status_code: 404,
            message: "not found".into(),
        }));
    }
}
