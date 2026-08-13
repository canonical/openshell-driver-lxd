// SPDX-License-Identifier: AGPL-3.0-or-later

//! Image alias lookups.

use urlencoding::encode;

use crate::client::LxdClient;
use crate::error::LxdError;

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
}
