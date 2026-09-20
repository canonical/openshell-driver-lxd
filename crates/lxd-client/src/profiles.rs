// SPDX-License-Identifier: AGPL-3.0-or-later

//! LXD profile lookups.

use urlencoding::encode;

use crate::client::LxdClient;
use crate::error::LxdError;
use crate::types::Profile;

impl LxdClient {
    /// `GET /1.0/profiles/<name>`: the profile as it exists in the client's
    /// project.
    pub async fn get_profile(&self, name: &str) -> Result<Profile, LxdError> {
        self.get::<Profile>(&format!("/1.0/profiles/{}", encode(name)))
            .await?
            .into_metadata()
    }
}
