// SPDX-License-Identifier: AGPL-3.0-or-later

//! Embedded DHCP client binary and event script assets.

use sha2::{Digest, Sha256};

/// Statically-linked one-shot DHCP client binary embedded at build time.
pub(crate) const DHCP_CLIENT_BINARY: &[u8] = include_bytes!("../assets/dhcp-client/udhcpc");

/// Event script passed to udhcpc via `-s` to apply IP address and route.
pub(crate) const DHCP_CLIENT_SCRIPT: &[u8] = include_bytes!("../assets/dhcp-client/udhcpc.script");

/// Computes a sha256 digest over the combined DHCP client binary and script bytes.
#[must_use]
pub(crate) fn dhcp_client_digest() -> String {
    let mut hasher = Sha256::new();
    hasher.update(DHCP_CLIENT_BINARY);
    hasher.update(DHCP_CLIENT_SCRIPT);
    format!("sha256:{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_are_non_empty_and_digest_is_valid() {
        assert!(!DHCP_CLIENT_BINARY.is_empty());
        assert!(!DHCP_CLIENT_SCRIPT.is_empty());
        let digest = dhcp_client_digest();
        assert!(digest.starts_with("sha256:"));
        assert_eq!(digest.len(), 7 + 64);
    }
}
