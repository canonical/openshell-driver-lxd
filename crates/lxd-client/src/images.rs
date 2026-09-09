// SPDX-License-Identifier: AGPL-3.0-or-later

//! Image alias lookups, creation, and split image imports.

use hyper::body::Bytes;
use serde_json::json;
use urlencoding::encode;

use crate::client::LxdClient;
use crate::error::LxdError;
use crate::types::Operation;

impl LxdClient {
    /// `GET /1.0/images/aliases/<alias>`: true if a local image alias
    /// resolves to an image, false if it doesn't exist.
    pub async fn image_alias_exists(&self, alias: &str) -> Result<bool, LxdError> {
        match self
            .get::<serde_json::Value>(&format!("/1.0/images/aliases/{}", encode(alias)))
            .await
        {
            Ok(_) => Ok(true),
            Err(LxdError::Api {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// `POST /1.0/images/aliases`: creates an image alias pointing to an existing image fingerprint.
    pub async fn create_image_alias(
        &self,
        alias: &str,
        target_fingerprint: &str,
        description: Option<&str>,
    ) -> Result<(), LxdError> {
        let body = json!({
            "name": alias,
            "target": target_fingerprint,
            "description": description.unwrap_or_default(),
        });
        self.post::<serde_json::Value>("/1.0/images/aliases", body)
            .await?;
        Ok(())
    }

    /// `POST /1.0/images`: imports a split image (metadata + rootfs) via multipart/form-data.
    ///
    /// Accepts raw bytes for `metadata` (e.g. `metadata.tar.xz`) and `rootfs`
    /// (e.g. `rootfs.squashfs` or `rootfs.tar.xz`), sends them as a multipart form,
    /// and returns an [`Operation`] tracking the import.
    pub async fn create_image_from_split(
        &self,
        metadata_filename: &str,
        metadata_bytes: &[u8],
        rootfs_filename: &str,
        rootfs_bytes: &[u8],
    ) -> Result<Operation, LxdError> {
        let boundary = "------------------------openshellsplitimageboundary";
        let mut body = Vec::with_capacity(metadata_bytes.len() + rootfs_bytes.len() + 1024);

        // metadata part
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"metadata\"; filename=\"{metadata_filename}\"\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(metadata_bytes);
        body.extend_from_slice(b"\r\n");

        // rootfs part
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"rootfs\"; filename=\"{rootfs_filename}\"\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(rootfs_bytes);
        body.extend_from_slice(b"\r\n");

        // closing boundary
        body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

        let content_type = format!("multipart/form-data; boundary={boundary}");
        let response = self
            .post_raw_response::<Operation>("/1.0/images", &content_type, &[], Bytes::from(body))
            .await?;
        response.into_metadata()
    }
}
