// SPDX-License-Identifier: AGPL-3.0-or-later

//! LXD project existence checks.

use urlencoding::encode;

use crate::client::LxdClient;
use crate::error::LxdError;

impl LxdClient {
    /// `GET /1.0/projects/<project>`: true if the project exists, false if it
    /// does not.
    pub async fn project_exists(&self, project: &str) -> Result<bool, LxdError> {
        match self
            .get::<serde_json::Value>(&format!("/1.0/projects/{}", encode(project)))
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
