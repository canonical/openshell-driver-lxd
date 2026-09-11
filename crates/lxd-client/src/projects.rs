// SPDX-License-Identifier: AGPL-3.0-or-later

//! LXD project management and existence checks.

use serde_json::json;
use urlencoding::encode;

use crate::client::LxdClient;
use crate::error::LxdError;
use crate::types::Operation;

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

    /// `POST /1.0/projects`: creates a new project.
    pub async fn create_project(&self, name: &str) -> Result<(), LxdError> {
        let body = json!({
            "name": name,
        });
        self.post::<serde_json::Value>("/1.0/projects", body)
            .await?;
        Ok(())
    }

    /// `DELETE /1.0/projects/<project>`: deletes a project.
    ///
    /// Waits for the deletion operation to complete. A 404 response is treated
    /// as success.
    pub async fn delete_project(&self, name: &str) -> Result<(), LxdError> {
        match self
            .delete::<Operation>(&format!("/1.0/projects/{}", encode(name)))
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
