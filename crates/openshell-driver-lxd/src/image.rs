// SPDX-License-Identifier: AGPL-3.0-or-later

//! OCI image resolution, caching, and LXD image importing.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

use lxd_client::LxdClient;
use regex::Regex;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::error::DriverError;

/// Default prefix for digest-derived LXD image aliases.
pub const DEFAULT_CACHE_ALIAS_PREFIX: &str = "openshell-oci-";

/// Canonical path inside the guest rootfs for the injected PID 1 init script.
pub(crate) const GUEST_INIT_SCRIPT_PATH: &str = "/openshell-init.sh";

/// Bundled POSIX/busybox init script injected into converted rootfs images.
const INIT_SCRIPT_CONTENTS: &str = include_str!("../assets/openshell-init.sh");

static OCI_REF_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Strict OCI reference grammar:
    // [docker://][domain[:port]/]path[:tag][@sha256:digest]
    // Rejects whitespace, control characters, shell metacharacters.
    Regex::new(r"^(?:docker://)?(?:(?:[a-zA-Z0-9.-]+(?::[0-9]+)?/)?(?:[a-z0-9]+(?:[._-][a-z0-9]+)*/)*[a-z0-9]+(?:[._-][a-z0-9]+)*)(?::[a-zA-Z0-9_.-]+)?(?:@sha256:[a-fA-F0-9]{64})?$")
        .expect("valid regex")
});

/// Strips an optional `docker://` transport prefix from a reference.
///
/// skopeo accepts both bare OCI references and `docker://`-prefixed ones, but
/// the importer always constructs its own `docker://` target, so any user-supplied
/// scheme must be removed before building that target.
pub fn strip_docker_scheme(reference: &str) -> &str {
    reference.strip_prefix("docker://").unwrap_or(reference)
}

/// Validates an OCI reference against a strict grammar.
///
/// Disallows shell metacharacters, whitespace, control characters, or invalid formats.
/// An optional leading `docker://` transport prefix is accepted.
///
/// A reference that fails this check can never be imported, so it is the
/// caller's mistake: [`DriverError::InvalidArgument`], not an import failure.
pub fn validate_reference(reference: &str) -> Result<(), DriverError> {
    if reference.is_empty() {
        return Err(DriverError::InvalidArgument(
            "image reference cannot be empty".to_string(),
        ));
    }
    if !OCI_REF_REGEX.is_match(reference) {
        return Err(DriverError::InvalidArgument(format!(
            "invalid OCI image reference: {reference:?}"
        )));
    }
    Ok(())
}

/// Returns the deterministic LXD cache alias for the given content digest.
///
/// Strips any `sha256:` prefix and returns `<prefix><full 64 hex chars>`.
/// Does not truncate the digest, ensuring distinct digests never collide.
pub fn cache_alias(digest: &str) -> String {
    cache_alias_with_prefix(DEFAULT_CACHE_ALIAS_PREFIX, digest)
}

/// Returns the cache alias using a custom prefix.
pub fn cache_alias_with_prefix(prefix: &str, digest: &str) -> String {
    let clean = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("{prefix}{clean}")
}

/// Returns `reference` with any leading `docker://` scheme and any trailing
/// `:tag` and/or `@sha256:<hex>` suffix stripped, leaving just
/// `[registry-host[:port]/]path`.
pub fn repo_path(reference: &str) -> &str {
    let reference = strip_docker_scheme(reference);

    // Strip trailing @sha256:<hex>
    let without_digest = if let Some(idx) = reference.rfind("@sha256:") {
        &reference[..idx]
    } else {
        reference
    };

    // Strip trailing :tag if present in the last path segment
    if let Some(colon_idx) = without_digest.rfind(':') {
        let last_slash = without_digest.rfind('/');
        match last_slash {
            Some(slash_idx) if colon_idx > slash_idx => &without_digest[..colon_idx],
            None => &without_digest[..colon_idx],
            _ => without_digest,
        }
    } else {
        without_digest
    }
}

/// Maps the current host architecture to the LXD architecture identifier
/// (used in an image's `metadata.yaml`).
pub fn host_lxd_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        "arm" => "armhf",
        "s390x" => "s390x",
        "powerpc64" => "ppc64el",
        "riscv64" => "riscv64",
        other => other,
    }
}

/// Maps the current host architecture to the OCI/Go architecture identifier
/// (what `skopeo --override-arch` expects when selecting an image from a
/// multi-arch index). This differs from [`host_lxd_arch`]: e.g. LXD calls
/// amd64 `x86_64`, but OCI image indexes use `amd64`.
/// Computes a `sha256:<hex>` digest of the file's bytes at the given path.
pub fn digest_of_file(path: &Path) -> Result<String, DriverError> {
    let bytes = std::fs::read(path).map_err(|e| {
        DriverError::ImageImport(format!(
            "failed to read supervisor binary at {}: {e}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest_hex = hex_digest(&hasher.finalize());
    Ok(format!("sha256:{digest_hex}"))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn host_oci_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        "s390x" => "s390x",
        "powerpc64" => "ppc64le",
        "riscv64" => "riscv64",
        other => other,
    }
}

/// Trait defining the external OCI pull/convert/import capability.
#[tonic::async_trait]
pub trait OciImporter: Send + Sync {
    /// Resolves `reference` to its arch-specific manifest digest (e.g. `sha256:abcdef...`).
    async fn resolve_digest(&self, reference: &str) -> Result<String, DriverError>;

    /// Imports the image identified by `digest` and registers it in LXD under `alias`.
    async fn import(&self, reference: &str, digest: &str, alias: &str) -> Result<(), DriverError>;

    /// Extracts the supervisor binary from `reference` into `cache_dir`, returning the host binary path and image digest.
    async fn extract_supervisor_binary(
        &self,
        reference: &str,
        cache_dir: &Path,
    ) -> Result<(PathBuf, String), DriverError>;
}

/// Abstraction over checking if an image alias exists in LXD.
#[tonic::async_trait]
pub trait ImageAliasChecker: Send + Sync {
    async fn image_alias_exists(&self, alias: &str) -> Result<bool, DriverError>;
}

#[tonic::async_trait]
impl ImageAliasChecker for LxdClient {
    async fn image_alias_exists(&self, alias: &str) -> Result<bool, DriverError> {
        self.image_alias_exists(alias).await.map_err(Into::into)
    }
}

/// Image cache resolver over LXD alias lookup and an `OciImporter`.
///
/// Ensures concurrent resolutions for the same alias are serialized,
/// importing missing images at most once.
#[derive(Clone)]
pub struct ImageCache {
    alias_checker: Arc<dyn ImageAliasChecker>,
    importer: Arc<dyn OciImporter>,
    prefix: String,
    locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl fmt::Debug for ImageCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageCache")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl ImageCache {
    pub fn new(lxd: LxdClient, importer: Arc<dyn OciImporter>, prefix: String) -> Self {
        Self::with_checker(Arc::new(lxd), importer, prefix)
    }

    pub fn with_checker(
        alias_checker: Arc<dyn ImageAliasChecker>,
        importer: Arc<dyn OciImporter>,
        prefix: String,
    ) -> Self {
        Self {
            alias_checker,
            importer,
            prefix,
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Resolves an OCI reference to a local LXD image alias.
    ///
    /// 1. Validates `reference`.
    /// 2. Resolves `reference` to its content digest (arch-specific).
    /// 3. Computes the deterministic cache alias.
    /// 4. Acquires the per-alias mutex.
    /// 5. Checks if the alias already exists in LXD (cache hit).
    /// 6. If not, calls `importer.import(reference, digest, alias)` (cache miss).
    /// 7. Returns the alias.
    pub async fn resolve_alias(&self, reference: &str) -> Result<String, DriverError> {
        validate_reference(reference)?;

        let digest = self.importer.resolve_digest(reference).await?;
        let alias = cache_alias_with_prefix(&self.prefix, &digest);

        let alias_lock = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(alias.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };

        let _guard = alias_lock.lock().await;

        if self.alias_checker.image_alias_exists(&alias).await? {
            tracing::debug!(alias = %alias, "image cache hit");
            return Ok(alias);
        }

        tracing::info!(reference = %reference, digest = %digest, alias = %alias, "image cache miss; importing");
        self.importer.import(reference, &digest, &alias).await?;
        Ok(alias)
    }

    /// Extracts the supervisor binary from `reference` into `cache_dir`, returning the host binary path and image digest.
    pub async fn extract_supervisor_binary(
        &self,
        reference: &str,
        cache_dir: &Path,
    ) -> Result<(PathBuf, String), DriverError> {
        self.importer
            .extract_supervisor_binary(reference, cache_dir)
            .await
    }
}

/// Selects the host-architecture manifest digest out of a raw top-level
/// manifest document.
///
/// `raw` is the bytes `skopeo inspect --raw` returned for the reference. For a
/// multi-arch index this picks the entry matching `os`/`arch` and returns its
/// digest; for a single manifest the digest *is* the content digest of these
/// very bytes, so it is computed directly.
///
/// Split out from the subprocess call so the arch-selection rule — the part
/// that silently produced cross-architecture cache collisions when it was
/// delegated to `skopeo inspect --format {{.Digest}}` — is unit-testable.
pub fn select_arch_digest(raw: &[u8], os: &str, arch: &str) -> Result<String, DriverError> {
    let doc: serde_json::Value = serde_json::from_slice(raw).map_err(|e| {
        DriverError::ImageImport(format!("failed to parse image manifest as JSON: {e}"))
    })?;

    let Some(manifests) = doc.get("manifests").and_then(|m| m.as_array()) else {
        // Not an index: this document is itself the image manifest, so its
        // own content digest identifies it.
        let mut hasher = Sha256::new();
        hasher.update(raw);
        return Ok(format!("sha256:{}", hex_digest(&hasher.finalize())));
    };

    for entry in manifests {
        let platform = entry.get("platform");
        let entry_os = platform.and_then(|p| p.get("os")).and_then(|v| v.as_str());
        let entry_arch = platform
            .and_then(|p| p.get("architecture"))
            .and_then(|v| v.as_str());
        if entry_os == Some(os) && entry_arch == Some(arch) {
            if let Some(digest) = entry.get("digest").and_then(|d| d.as_str()) {
                return Ok(digest.to_string());
            }
        }
    }

    Err(DriverError::ImageImport(format!(
        "image index has no {os}/{arch} manifest"
    )))
}

/// Real implementation of [`OciImporter`] using `skopeo`, `umoci`, and `mksquashfs`.
pub struct SkopeoImporter {
    lxd: LxdClient,
    skopeo_path: PathBuf,
    umoci_path: PathBuf,
    mksquashfs_path: PathBuf,
    work_dir: PathBuf,
    timeout: Duration,
}

impl SkopeoImporter {
    pub fn new(
        lxd: LxdClient,
        skopeo_path: Option<PathBuf>,
        umoci_path: Option<PathBuf>,
        mksquashfs_path: Option<PathBuf>,
        work_dir: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self {
            lxd,
            skopeo_path: skopeo_path.unwrap_or_else(|| PathBuf::from("skopeo")),
            umoci_path: umoci_path.unwrap_or_else(|| PathBuf::from("umoci")),
            mksquashfs_path: mksquashfs_path.unwrap_or_else(|| PathBuf::from("mksquashfs")),
            work_dir,
            timeout,
        }
    }

    /// Creates the scratch directory for one conversion inside the configured
    /// work dir, rather than `TMPDIR`/`/tmp`, which is a small tmpfs on most
    /// modern distributions and cannot hold an unpacked sandbox rootfs.
    fn scratch_dir(&self, prefix: &str) -> Result<tempfile::TempDir, DriverError> {
        std::fs::create_dir_all(&self.work_dir).map_err(|e| {
            DriverError::ImageImport(format!(
                "failed to create image work directory {}: {e}",
                self.work_dir.display()
            ))
        })?;
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(&self.work_dir)
            .map_err(|e| {
                DriverError::ImageImport(format!(
                    "failed to create scratch directory in {}: {e}",
                    self.work_dir.display()
                ))
            })
    }
}

#[tonic::async_trait]
impl OciImporter for SkopeoImporter {
    async fn resolve_digest(&self, reference: &str) -> Result<String, DriverError> {
        let bare = strip_docker_scheme(reference);
        let target = format!("docker://{bare}");

        // `--raw` returns the top-level manifest document untouched. Two
        // reasons to prefer it over `inspect --format {{.Digest}}`:
        //
        //  * `{{.Digest}}` reports the digest of the *index* even under
        //    `--override-arch`, so every architecture resolved to the same
        //    digest and shared one cache alias.
        //  * a plain `inspect` also paginates the repository's whole tag
        //    list, which costs ~13s on a repo with thousands of tags and is
        //    pure waste when only the digest is wanted.
        let cmd = tokio::process::Command::new(&self.skopeo_path)
            .args(["inspect", "--raw", &target])
            .kill_on_drop(true)
            .output();

        let output = tokio::time::timeout(self.timeout, cmd)
            .await
            .map_err(|_| {
                DriverError::ImageImport(format!("skopeo inspect timed out for {reference:?}"))
            })?
            .map_err(|e| DriverError::ImageImport(format!("failed to execute skopeo: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DriverError::ImageImport(format!(
                "skopeo inspect failed for {reference:?}: {stderr}"
            )));
        }

        select_arch_digest(&output.stdout, "linux", host_oci_arch()).map_err(|e| {
            DriverError::ImageImport(format!("failed to resolve digest for {reference:?}: {e}"))
        })
    }

    async fn import(&self, reference: &str, digest: &str, alias: &str) -> Result<(), DriverError> {
        let oci_arch = host_oci_arch();
        let lxd_arch = host_lxd_arch();
        let repo = repo_path(reference);
        let copy_source = format!("docker://{repo}@{digest}");

        // Create scratch directory for conversion, on disk rather than tmpfs.
        let temp_dir = self.scratch_dir("openshell-oci-import-")?;
        let temp_path = temp_dir.path();

        let oci_dest = temp_path.join("oci");
        let bundle_dest = temp_path.join("bundle");
        let rootfs_dest = bundle_dest.join("rootfs");
        let squashfs_path = temp_path.join("rootfs.squashfs");

        // 1. skopeo copy docker://<repo>@<digest> oci:<temp_path>/oci:img
        let oci_tag_arg = format!("oci:{}:img", oci_dest.display());
        let cmd = tokio::process::Command::new(&self.skopeo_path)
            .args([
                "copy",
                "--override-os",
                "linux",
                "--override-arch",
                oci_arch,
                &copy_source,
                &oci_tag_arg,
            ])
            .kill_on_drop(true)
            .output();

        let output = tokio::time::timeout(self.timeout, cmd)
            .await
            .map_err(|_| {
                DriverError::ImageImport(format!("skopeo copy timed out for {copy_source:?}"))
            })?
            .map_err(|e| DriverError::ImageImport(format!("failed to execute skopeo copy: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DriverError::ImageImport(format!(
                "skopeo copy failed for {copy_source:?}: {stderr}"
            )));
        }

        // 2. umoci unpack --image <temp_path>/oci:img <temp_path>/bundle
        let cmd = tokio::process::Command::new(&self.umoci_path)
            .args([
                "unpack",
                "--rootless",
                "--image",
                &format!("{}:img", oci_dest.display()),
                &bundle_dest.display().to_string(),
            ])
            .kill_on_drop(true)
            .output();

        let output = tokio::time::timeout(self.timeout, cmd)
            .await
            .map_err(|_| DriverError::ImageImport(format!("umoci unpack timed out for {alias}")))?
            .map_err(|e| {
                DriverError::ImageImport(format!("failed to execute umoci unpack: {e}"))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DriverError::ImageImport(format!(
                "umoci unpack failed for {alias}: {stderr}"
            )));
        }

        // The compressed OCI copy has served its purpose now that the rootfs
        // is unpacked. Dropping it here keeps peak scratch usage to the
        // rootfs plus the squashfs being written, instead of holding all
        // three representations of the image at once.
        if let Err(e) = tokio::fs::remove_dir_all(&oci_dest).await {
            tracing::debug!(path = %oci_dest.display(), %e, "could not free OCI copy early");
        }

        // Inject the minimal init script before repacking with mksquashfs.
        inject_init_script(&rootfs_dest)?;

        // 3. mksquashfs <rootfs_dest> <squashfs_path> -noappend
        let cmd = tokio::process::Command::new(&self.mksquashfs_path)
            .args([
                rootfs_dest.as_os_str(),
                squashfs_path.as_os_str(),
                std::ffi::OsStr::new("-noappend"),
            ])
            .kill_on_drop(true)
            .output();

        let output = tokio::time::timeout(self.timeout, cmd)
            .await
            .map_err(|_| DriverError::ImageImport(format!("mksquashfs timed out for {alias}")))?
            .map_err(|e| DriverError::ImageImport(format!("failed to execute mksquashfs: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DriverError::ImageImport(format!(
                "mksquashfs failed for {alias}: {stderr}"
            )));
        }

        // 4. Assemble metadata.tar.xz
        // metadata.yaml contains architecture and creation date
        let creation_date = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let metadata_yaml = format!(
            "architecture: \"{lxd_arch}\"\ncreation_date: {creation_date}\nproperties:\n  description: \"OpenShell sandbox {alias}\"\n"
        );

        let metadata_tar_bytes = create_metadata_tar_xz(temp_path, &metadata_yaml).await?;

        // 5. Upload via LxdClient::create_image_from_split + wait_operation
        let op = self
            .lxd
            .create_image_from_split(
                "metadata.tar.xz",
                &metadata_tar_bytes,
                "rootfs.squashfs",
                &squashfs_path,
            )
            .await
            .map_err(|e| DriverError::ImageImport(format!("LXD split image upload failed: {e}")))?;

        let finished_op = tokio::time::timeout(self.timeout, self.lxd.wait_operation(&op.id))
            .await
            .map_err(|_| {
                DriverError::ImageImport("timed out waiting for image upload operation".into())
            })?
            .map_err(|e| DriverError::ImageImport(format!("image upload operation failed: {e}")))?;

        let fingerprint = finished_op
            .metadata
            .as_ref()
            .and_then(|m| m.get("fingerprint"))
            .and_then(|f| f.as_str())
            .ok_or_else(|| {
                DriverError::ImageImport("operation response missing image fingerprint".into())
            })?;

        // 6. Bind the alias to the fingerprint
        self.lxd
            .create_image_alias(
                alias,
                fingerprint,
                Some(&format!("OpenShell image {digest}")),
            )
            .await
            .map_err(|e| {
                DriverError::ImageImport(format!("failed to create image alias {alias}: {e}"))
            })?;

        Ok(())
    }

    async fn extract_supervisor_binary(
        &self,
        reference: &str,
        cache_dir: &Path,
    ) -> Result<(PathBuf, String), DriverError> {
        validate_reference(reference)?;
        let digest = self.resolve_digest(reference).await?;
        let clean_digest = digest.strip_prefix("sha256:").unwrap_or(&digest);
        let target_dir = cache_dir.join(clean_digest);
        let binary_path = target_dir.join("openshell-sandbox");

        if binary_path.exists() {
            tracing::debug!(path = %binary_path.display(), "supervisor binary cache hit");
            return Ok((binary_path, digest));
        }

        tracing::info!(
            reference = %reference,
            digest = %digest,
            "supervisor binary cache miss; extracting"
        );

        let oci_arch = host_oci_arch();
        let repo = repo_path(reference);
        let copy_source = format!("docker://{repo}@{digest}");

        let temp_dir = self.scratch_dir("openshell-supervisor-extract-")?;
        let temp_path = temp_dir.path();

        let oci_dest = temp_path.join("oci");
        let bundle_dest = temp_path.join("bundle");
        let rootfs_dest = bundle_dest.join("rootfs");

        // 1. skopeo copy docker://<repo>@<digest> oci:<temp_path>/oci:img
        let oci_tag_arg = format!("oci:{}:img", oci_dest.display());
        let cmd = tokio::process::Command::new(&self.skopeo_path)
            .args([
                "copy",
                "--override-os",
                "linux",
                "--override-arch",
                oci_arch,
                &copy_source,
                &oci_tag_arg,
            ])
            .kill_on_drop(true)
            .output();

        let output = tokio::time::timeout(self.timeout, cmd)
            .await
            .map_err(|_| {
                DriverError::ImageImport(format!("skopeo copy timed out for {copy_source:?}"))
            })?
            .map_err(|e| DriverError::ImageImport(format!("failed to execute skopeo copy: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DriverError::ImageImport(format!(
                "skopeo copy failed for {copy_source:?}: {stderr}"
            )));
        }

        // 2. umoci unpack --image <temp_path>/oci:img <temp_path>/bundle
        let cmd = tokio::process::Command::new(&self.umoci_path)
            .args([
                "unpack",
                "--rootless",
                "--image",
                &format!("{}:img", oci_dest.display()),
                &bundle_dest.display().to_string(),
            ])
            .kill_on_drop(true)
            .output();

        let output = tokio::time::timeout(self.timeout, cmd)
            .await
            .map_err(|_| {
                DriverError::ImageImport(format!("umoci unpack timed out for {reference}"))
            })?
            .map_err(|e| {
                DriverError::ImageImport(format!("failed to execute umoci unpack: {e}"))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DriverError::ImageImport(format!(
                "umoci unpack failed for {reference}: {stderr}"
            )));
        }

        let extracted_source = rootfs_dest.join("openshell-sandbox");
        if !extracted_source.exists() {
            return Err(DriverError::ImageImport(format!(
                "image {reference:?} does not contain /openshell-sandbox"
            )));
        }

        tokio::fs::create_dir_all(&target_dir).await.map_err(|e| {
            DriverError::ImageImport(format!(
                "failed to create supervisor cache directory {}: {e}",
                target_dir.display()
            ))
        })?;

        // Unique per call, not just per process: two concurrent creates in
        // this process extracting the same digest would otherwise race on one
        // staging path and one of them would rename a half-written file.
        static EXTRACT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temp_target = target_dir.join(format!(
            ".openshell-sandbox.tmp.{}.{}",
            std::process::id(),
            EXTRACT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        tokio::fs::copy(&extracted_source, &temp_target)
            .await
            .map_err(|e| {
                DriverError::ImageImport(format!(
                    "failed to copy extracted supervisor binary to {}: {e}",
                    temp_target.display()
                ))
            })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                tokio::fs::set_permissions(&temp_target, std::fs::Permissions::from_mode(0o755))
                    .await;
        }

        tokio::fs::rename(&temp_target, &binary_path)
            .await
            .map_err(|e| {
                DriverError::ImageImport(format!(
                    "failed to commit extracted supervisor binary to {}: {e}",
                    binary_path.display()
                ))
            })?;

        Ok((binary_path, digest))
    }
}

/// Creates a `.tar.xz` archive containing `metadata.yaml` in the given directory.
async fn create_metadata_tar_xz(dir: &Path, metadata_yaml: &str) -> Result<Vec<u8>, DriverError> {
    let metadata_path = dir.join("metadata.yaml");
    let tar_path = dir.join("metadata.tar.xz");

    tokio::fs::write(&metadata_path, metadata_yaml.as_bytes())
        .await
        .map_err(|e| DriverError::ImageImport(format!("failed to write metadata.yaml: {e}")))?;

    let output = tokio::process::Command::new("tar")
        .arg("-cJf")
        .arg(&tar_path)
        .arg("-C")
        .arg(dir)
        .arg("metadata.yaml")
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| DriverError::ImageImport(format!("failed to run tar for metadata: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(DriverError::ImageImport(format!(
            "failed to build metadata.tar.xz: {stderr}"
        )));
    }

    tokio::fs::read(&tar_path)
        .await
        .map_err(|e| DriverError::ImageImport(format!("failed to read metadata.tar.xz: {e}")))
}

/// Injects the bundled init script into the unpacked rootfs at `GUEST_INIT_SCRIPT_PATH`
/// with executable permissions (`0755`).
fn inject_init_script(rootfs_dest: &Path) -> Result<(), DriverError> {
    let script_path = rootfs_dest.join(GUEST_INIT_SCRIPT_PATH.trim_start_matches('/'));
    if let Some(parent) = script_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            DriverError::ImageImport(format!(
                "failed to create parent directory for init script: {e}"
            ))
        })?;
    }
    std::fs::write(&script_path, INIT_SCRIPT_CONTENTS).map_err(|e| {
        DriverError::ImageImport(format!(
            "failed to write init script to {}: {e}",
            script_path.display()
        ))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).map_err(
            |e| {
                DriverError::ImageImport(format!(
                    "failed to set permissions on init script {}: {e}",
                    script_path.display()
                ))
            },
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_alias_golden() {
        let digest_body = "ab".repeat(32);
        let digest = format!("sha256:{digest_body}");
        let alias = cache_alias(&digest);
        assert_eq!(alias, format!("openshell-oci-{digest_body}"));
        assert_eq!(alias.len(), "openshell-oci-".len() + 64);
    }

    #[test]
    fn cache_alias_uniqueness() {
        let d1 = format!("sha256:{}", "01".repeat(32));
        let d2 = format!("sha256:{}", "02".repeat(32));
        assert_ne!(cache_alias(&d1), cache_alias(&d2));
    }

    #[test]
    fn reference_validation() {
        // Valid references
        assert!(validate_reference("ubuntu").is_ok());
        assert!(validate_reference("ubuntu:22.04").is_ok());
        assert!(validate_reference("registry.example.com/openshell-sandbox:test").is_ok());
        assert!(validate_reference("registry.example.com:5000/repo/app:v1.0").is_ok());
        let full_digest = format!("registry.example.com/app@sha256:{}", "ab".repeat(32));
        assert!(validate_reference(&full_digest).is_ok());
        let tagged_and_digested = format!("registry.example.com/app:v1@sha256:{}", "ab".repeat(32));
        assert!(validate_reference(&tagged_and_digested).is_ok());
        // Optional docker:// transport prefix is accepted.
        assert!(validate_reference("docker://ubuntu:22.04").is_ok());
        assert!(validate_reference("docker://registry.example.com/org/sandbox:latest").is_ok());

        // Invalid references (shell injection / malformed) are the caller's
        // mistake.
        for invalid in [
            "",
            "ubuntu; rm -rf /",
            "ubuntu && touch /tmp/pwn",
            "ubuntu|cat",
            "ubuntu`id`",
            "ubuntu$(id)",
            "ubuntu\n",
            "ubuntu foo",
            "ubuntu:",
            "UPPER/Case::bad",
        ] {
            assert!(
                matches!(
                    validate_reference(invalid),
                    Err(DriverError::InvalidArgument(_))
                ),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn repo_extraction() {
        let digest_hex = "ab".repeat(32);
        assert_eq!(repo_path("ubuntu"), "ubuntu");
        assert_eq!(repo_path("ubuntu:22.04"), "ubuntu");
        assert_eq!(
            repo_path("registry.example.com:5000/app:latest"),
            "registry.example.com:5000/app"
        );
        assert_eq!(
            repo_path(&format!("registry.example.com/foo@sha256:{digest_hex}")),
            "registry.example.com/foo"
        );
        assert_eq!(
            repo_path(&format!(
                "registry.example.com:5000/foo:v1@sha256:{digest_hex}"
            )),
            "registry.example.com:5000/foo"
        );
        // docker:// scheme is stripped along with tag/digest suffixes.
        assert_eq!(
            repo_path("docker://registry.example.com/org/sandbox:latest"),
            "registry.example.com/org/sandbox"
        );
        assert_eq!(
            repo_path(&format!(
                "docker://registry.example.com/org/sandbox@sha256:{digest_hex}"
            )),
            "registry.example.com/org/sandbox"
        );
    }

    /// A multi-arch index must resolve to a *different* digest per
    /// architecture. Regression guard: `skopeo inspect --format {{.Digest}}`
    /// returns the index digest even under `--override-arch`, so amd64 and
    /// arm64 both mapped to one cache alias and one host could serve another
    /// architecture's image out of the cache.
    #[test]
    fn arch_digest_differs_per_architecture() {
        let index = br#"{
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                {"digest": "sha256:aaaa", "platform": {"os": "linux", "architecture": "amd64"}},
                {"digest": "sha256:bbbb", "platform": {"os": "linux", "architecture": "arm64"}},
                {"digest": "sha256:cccc", "platform": {"os": "unknown", "architecture": "unknown"}}
            ]
        }"#;

        let amd = select_arch_digest(index, "linux", "amd64").unwrap();
        let arm = select_arch_digest(index, "linux", "arm64").unwrap();
        assert_eq!(amd, "sha256:aaaa");
        assert_eq!(arm, "sha256:bbbb");
        assert_ne!(amd, arm);

        // An architecture the index does not carry is an error, not a
        // silent fallback to some other arch's manifest.
        assert!(select_arch_digest(index, "linux", "riscv64").is_err());
    }

    #[test]
    fn single_manifest_digest_is_content_digest() {
        // Not an index: the digest is the sha256 of the document itself.
        let raw = br#"{"mediaType":"application/vnd.oci.image.manifest.v1+json","layers":[]}"#;
        let digest = select_arch_digest(raw, "linux", "amd64").unwrap();

        let mut hasher = Sha256::new();
        hasher.update(raw);
        assert_eq!(digest, format!("sha256:{}", hex_digest(&hasher.finalize())));
    }

    #[test]
    fn malformed_manifest_is_rejected() {
        assert!(select_arch_digest(b"not json", "linux", "amd64").is_err());
    }

    #[test]
    fn host_lxd_arch_is_valid() {
        let arch = host_lxd_arch();
        assert!(!arch.is_empty());
    }

    #[test]
    fn host_oci_arch_uses_oci_names() {
        // skopeo selects from a multi-arch index using OCI/Go arch names, not
        // LXD's (e.g. amd64, not x86_64), so the two must not be conflated.
        let arch = host_oci_arch();
        assert!(!arch.is_empty());
        assert_ne!(arch, "x86_64");
        assert_ne!(arch, "aarch64");
    }

    struct MockImporter {
        digest_to_return: String,
        import_calls: std::sync::atomic::AtomicUsize,
        extract_calls: std::sync::atomic::AtomicUsize,
        recorded_repo_digest: std::sync::Mutex<Vec<(String, String)>>,
        import_delay: Duration,
    }

    #[tonic::async_trait]
    impl OciImporter for MockImporter {
        async fn resolve_digest(&self, _reference: &str) -> Result<String, DriverError> {
            Ok(self.digest_to_return.clone())
        }

        async fn import(
            &self,
            reference: &str,
            digest: &str,
            _alias: &str,
        ) -> Result<(), DriverError> {
            self.import_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.recorded_repo_digest
                .lock()
                .unwrap()
                .push((reference.to_string(), digest.to_string()));
            if !self.import_delay.is_zero() {
                tokio::time::sleep(self.import_delay).await;
            }
            Ok(())
        }

        async fn extract_supervisor_binary(
            &self,
            _reference: &str,
            cache_dir: &Path,
        ) -> Result<(PathBuf, String), DriverError> {
            self.extract_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let clean = self
                .digest_to_return
                .strip_prefix("sha256:")
                .unwrap_or(&self.digest_to_return);
            let target_dir = cache_dir.join(clean);
            let binary_path = target_dir.join("openshell-sandbox");
            if binary_path.exists() {
                return Ok((binary_path, self.digest_to_return.clone()));
            }
            std::fs::create_dir_all(&target_dir).map_err(|e| {
                DriverError::ImageImport(format!("failed to create cache dir: {e}"))
            })?;
            std::fs::write(&binary_path, b"mock-supervisor-binary").map_err(|e| {
                DriverError::ImageImport(format!("failed to write mock binary: {e}"))
            })?;
            Ok((binary_path, self.digest_to_return.clone()))
        }
    }

    struct MockAliasChecker {
        existing_aliases: std::sync::Mutex<std::collections::HashSet<String>>,
    }

    #[tonic::async_trait]
    impl ImageAliasChecker for MockAliasChecker {
        async fn image_alias_exists(&self, alias: &str) -> Result<bool, DriverError> {
            Ok(self.existing_aliases.lock().unwrap().contains(alias))
        }
    }

    #[tokio::test]
    async fn cache_hit_and_miss_and_concurrency() {
        let digest_hex = "cc".repeat(32);
        let digest = format!("sha256:{digest_hex}");
        let importer = Arc::new(MockImporter {
            digest_to_return: digest.clone(),
            import_calls: std::sync::atomic::AtomicUsize::new(0),
            extract_calls: std::sync::atomic::AtomicUsize::new(0),
            recorded_repo_digest: std::sync::Mutex::new(Vec::new()),
            import_delay: Duration::from_millis(50),
        });

        let alias_checker = Arc::new(MockAliasChecker {
            existing_aliases: std::sync::Mutex::new(std::collections::HashSet::new()),
        });

        let prefix = "test-oci-".to_string();
        let cache =
            ImageCache::with_checker(alias_checker.clone(), importer.clone(), prefix.clone());

        // 1. Initial resolution is a miss -> calls importer.import once
        let res_alias = cache.resolve_alias("ubuntu:22.04").await.unwrap();
        let expected_alias = format!("test-oci-{digest_hex}");
        assert_eq!(res_alias, expected_alias);
        assert_eq!(
            importer
                .import_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        // Mark alias as now existing in LXD
        alias_checker
            .existing_aliases
            .lock()
            .unwrap()
            .insert(expected_alias.clone());

        // 2. Second resolution is a hit -> does not call importer.import again
        let res_alias2 = cache.resolve_alias("ubuntu:22.04").await.unwrap();
        assert_eq!(res_alias2, expected_alias);
        assert_eq!(
            importer
                .import_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        // 3. Concurrency guard test: clear existing aliases, launch 5 concurrent resolves
        alias_checker.existing_aliases.lock().unwrap().clear();
        importer
            .import_calls
            .store(0, std::sync::atomic::Ordering::SeqCst);

        // A custom mock alias checker that sets the alias upon import
        struct AutoImportingImporter {
            inner: Arc<MockImporter>,
            checker: Arc<MockAliasChecker>,
        }

        #[tonic::async_trait]
        impl OciImporter for AutoImportingImporter {
            async fn resolve_digest(&self, ref_str: &str) -> Result<String, DriverError> {
                self.inner.resolve_digest(ref_str).await
            }

            async fn import(
                &self,
                reference: &str,
                digest: &str,
                alias: &str,
            ) -> Result<(), DriverError> {
                self.inner.import(reference, digest, alias).await?;
                self.checker
                    .existing_aliases
                    .lock()
                    .unwrap()
                    .insert(alias.to_string());
                Ok(())
            }

            async fn extract_supervisor_binary(
                &self,
                reference: &str,
                cache_dir: &Path,
            ) -> Result<(PathBuf, String), DriverError> {
                self.inner
                    .extract_supervisor_binary(reference, cache_dir)
                    .await
            }
        }

        let auto_importer = Arc::new(AutoImportingImporter {
            inner: importer.clone(),
            checker: alias_checker.clone(),
        });

        let cache = Arc::new(ImageCache::with_checker(
            alias_checker.clone(),
            auto_importer,
            prefix,
        ));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let cache_clone = cache.clone();
            handles.push(tokio::spawn(async move {
                cache_clone.resolve_alias("ubuntu:22.04").await
            }));
        }

        for handle in handles {
            let res = handle.await.unwrap().unwrap();
            assert_eq!(res, expected_alias);
        }

        // Exactly 1 import call occurred among the 5 concurrent requests
        assert_eq!(
            importer
                .import_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn digest_import_consistency() {
        // Assert that the copy target uses the resolved digest rather than original tag
        let ref_with_tag = "registry.example.com/org/app:v1.2.3";
        let resolved_digest = format!("sha256:{}", "ff".repeat(32));
        let repo = repo_path(ref_with_tag);
        assert_eq!(repo, "registry.example.com/org/app");
        let copy_source = format!("docker://{repo}@{resolved_digest}");
        assert_eq!(
            copy_source,
            format!(
                "docker://registry.example.com/org/app@sha256:{}",
                "ff".repeat(32)
            )
        );

        // A user-supplied docker:// scheme must not produce a doubled scheme.
        let ref_with_scheme = "docker://registry.example.com/org/app:v1.2.3";
        let repo_with_scheme = repo_path(ref_with_scheme);
        assert_eq!(repo_with_scheme, "registry.example.com/org/app");
        let copy_source_with_scheme = format!("docker://{repo_with_scheme}@{resolved_digest}");
        assert_eq!(copy_source_with_scheme, copy_source);
    }

    #[test]
    fn test_inject_init_script() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let rootfs = temp_dir.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        inject_init_script(&rootfs).unwrap();

        let expected_path = rootfs.join(GUEST_INIT_SCRIPT_PATH.trim_start_matches('/'));
        assert!(expected_path.exists());
        let contents = std::fs::read_to_string(&expected_path).unwrap();
        assert!(!contents.is_empty());
        assert_eq!(contents, INIT_SCRIPT_CONTENTS);

        #[cfg(unix)]
        {
            let metadata = std::fs::metadata(&expected_path).unwrap();
            let mode = metadata.permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
    }

    #[tokio::test]
    async fn extract_supervisor_binary_cache_hit_and_miss() {
        let digest_hex = "ee".repeat(32);
        let digest = format!("sha256:{digest_hex}");
        let importer = Arc::new(MockImporter {
            digest_to_return: digest.clone(),
            import_calls: std::sync::atomic::AtomicUsize::new(0),
            extract_calls: std::sync::atomic::AtomicUsize::new(0),
            recorded_repo_digest: std::sync::Mutex::new(Vec::new()),
            import_delay: Duration::ZERO,
        });

        let alias_checker = Arc::new(MockAliasChecker {
            existing_aliases: std::sync::Mutex::new(std::collections::HashSet::new()),
        });

        let cache =
            ImageCache::with_checker(alias_checker, importer.clone(), "test-oci-".to_string());

        let temp_cache_dir = tempfile::tempdir().unwrap();

        // 1. First extraction is a cache miss
        let (bin_path1, dig1) = cache
            .extract_supervisor_binary(
                "ghcr.io/nvidia/openshell/supervisor:latest",
                temp_cache_dir.path(),
            )
            .await
            .unwrap();
        assert_eq!(dig1, digest);
        assert!(bin_path1.exists());
        assert_eq!(
            importer
                .extract_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );

        // 2. Second extraction for same digest is a cache hit (calls extract_supervisor_binary on importer, but hits file check)
        let (bin_path2, dig2) = cache
            .extract_supervisor_binary(
                "ghcr.io/nvidia/openshell/supervisor:latest",
                temp_cache_dir.path(),
            )
            .await
            .unwrap();
        assert_eq!(dig2, digest);
        assert_eq!(bin_path1, bin_path2);
    }

    #[test]
    fn test_digest_of_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test_bin");
        std::fs::write(&file_path, b"hello supervisor").unwrap();

        let digest = digest_of_file(&file_path).unwrap();
        // SHA-256("hello supervisor") = 13698ec9ad86f380ac98b76b758f96f23ed83b0107115f4dfee415be8a26fe38
        assert_eq!(
            digest,
            "sha256:13698ec9ad86f380ac98b76b758f96f23ed83b0107115f4dfee415be8a26fe38"
        );
    }
}
