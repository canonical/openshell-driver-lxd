// SPDX-License-Identifier: AGPL-3.0-or-later

//! OCI image resolution, caching, and LXD image importing.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
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

/// Bundled POSIX/busybox init script injected into converted rootfs images.
const INIT_SCRIPT_CONTENTS: &str = include_str!("../assets/openshell-init.sh");

/// Name of the in-workload boundary binary the supervisor image ships at its
/// root, extracted into a host cache and mounted into the workload container.
const SUPERVISOR_BINARY_NAME: &str = "openshell-sandbox";

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

/// The registry host a reference names, or None when it names none.
///
/// Follows the Docker rule the grammar in `OCI_REF_REGEX` encodes: the first
/// path segment is a registry host only when it looks like one — it carries a
/// dot or a port. Otherwise the reference is a Docker Hub path.
pub fn registry_host(reference: &str) -> Option<&str> {
    let bare = strip_docker_scheme(reference);
    let (first, _) = bare.split_once('/')?;
    (first.contains('.') || first.contains(':')).then_some(first)
}

/// Checks `reference` against the configured registry allowlist.
///
/// An empty allowlist allows every registry, which is the driver's default and
/// lets a sandbox request reach whatever the host network reaches. With one
/// configured, a reference naming no host is refused too: it would resolve to
/// Docker Hub, which is a registry like any other.
pub fn check_registry_allowed(reference: &str, allowed: &[String]) -> Result<(), DriverError> {
    if allowed.is_empty() {
        return Ok(());
    }
    match registry_host(reference) {
        Some(host) if allowed.iter().any(|a| a.eq_ignore_ascii_case(host)) => Ok(()),
        Some(host) => Err(DriverError::InvalidArgument(format!(
            "image registry {host:?} is not one this driver may pull from ({})",
            allowed.join(", ")
        ))),
        None => Err(DriverError::InvalidArgument(format!(
            "image reference {reference:?} names no registry, and this driver may pull \
             only from {}",
            allowed.join(", ")
        ))),
    }
}

/// Cache size `sqfstar` is allowed for an image conversion.
///
/// See the call site: the default (a quarter of physical RAM) is too much for
/// a host that is also running the gateway and its sandboxes.
const SQFSTAR_MEM: &str = "512M";

/// Revision of the OCI-to-LXD conversion, part of every cache alias.
///
/// Bump it whenever the conversion produces a different image for the same
/// OCI digest, so images converted the old way are imported again instead of
/// being reused. Revision 2 keeps the image's file ownership; revision 3 boots
/// the init script through `/sbin/init`, which also takes the nameserver from
/// the network, reaches gateways by IPv6 address or host name and probes an
/// https gateway the way the supervisor connects to it; revision 4 drops the
/// init script's public-resolver fallback, so a sandbox whose network offers
/// no name server has none rather than a silent one.
///
/// Revision 5 is the RFC 0012 dual-container sandbox: the init script runs
/// entirely out of the driver's own volumes — its interpreter included — and
/// hands the in-workload boundary its bootstrap through
/// `launch-capability-free`, which empties every capability set, sets
/// `no_new_privs` and drops to the resolved workload identity, all of which
/// the boundary verifies before running anything. It allows unprivileged
/// low-port binds for the boundary's DNS relay, and keeps the boundary's own
/// files in a directory it owns so it can delete its one-use bootstrap.
///
/// Revisions 6 to 9 are deliberately skipped. Pre-release builds of this work
/// used them for conversions that differ from what revision 11 produces, so a
/// host that converted an image under one of those must not reuse it;
/// advancing past them all is what makes that impossible rather than
/// unlikely.
///
/// Revision 11 answers a loopback gateway host without asking /etc/hosts or
/// DNS: the default sandbox image ships an empty /etc/hosts and the pinned
/// supervisor image ships no getent, so resolving `localhost` through the
/// system depended on the network's resolver accepting a special-use name,
/// which RFC 6761 says it need not.
///
/// Revision 12 replaces the unpack-and-repack conversion with a streaming one:
/// the OCI layers are merged in memory and piped straight into `sqfstar`,
/// instead of being unpacked with `umoci` and repacked with `mksquashfs`.
/// Setuid/setgid bits now survive (the old `--rootless` unpack could not
/// create them under the snap's seccomp policy), and the image is no longer
/// rewritten, so the same OCI digest converts to a different image.
///
/// The init script is baked into the converted image, so any change to
/// `assets/openshell-init.sh` needs a bump here: without one, hosts that
/// already converted an image keep booting the old script.
pub const CONVERSION_REVISION: u32 = 12;

/// Returns the deterministic LXD cache alias for the given content digest.
///
/// Strips any `sha256:` prefix and returns
/// `<prefix>r<conversion revision>-<full 64 hex chars>`. Does not truncate the
/// digest, ensuring distinct digests never collide.
pub fn cache_alias(digest: &str) -> String {
    cache_alias_with_prefix(DEFAULT_CACHE_ALIAS_PREFIX, digest)
}

/// Returns the cache alias using a custom prefix.
pub fn cache_alias_with_prefix(prefix: &str, digest: &str) -> String {
    let clean = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("{prefix}r{CONVERSION_REVISION}-{clean}")
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

/// The `docker://` target `skopeo inspect` resolves `reference` through.
///
/// skopeo rejects a reference carrying both a tag and a digest ("Docker
/// references with both a tag and digest are currently not supported"), but
/// such a reference is valid and the most readable way to pin an image. The
/// digest identifies the image on its own, so the tag is dropped.
pub fn inspect_target(reference: &str) -> String {
    let bare = strip_docker_scheme(reference);
    match bare.rfind("@sha256:") {
        Some(idx) => format!("docker://{}{}", repo_path(bare), &bare[idx..]),
        None => format!("docker://{bare}"),
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
    let metadata = std::fs::symlink_metadata(path).map_err(|e| {
        DriverError::ImageImport(format!(
            "failed to stat supervisor binary at {}: {e}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DriverError::ImageImport(format!(
            "supervisor binary at {} is a symlink or not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| {
                DriverError::ImageImport(format!(
                    "failed to open supervisor binary at {}: {e}",
                    path.display()
                ))
            })?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes).map_err(|e| {
            DriverError::ImageImport(format!(
                "failed to read supervisor binary at {}: {e}",
                path.display()
            ))
        })?;
        bytes
    };
    #[cfg(not(unix))]
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
#[allow(clippy::double_must_use)]
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
#[allow(clippy::double_must_use)]
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
    supervisor_extract_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
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
            supervisor_extract_locks: Arc::new(Mutex::new(HashMap::new())),
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

    /// The cache alias `reference` resolves to, without importing anything.
    ///
    /// Only the registry round trip that maps the reference to a digest runs,
    /// so this is the cheap way to ask "which local image is this reference".
    pub async fn cache_alias_for(&self, reference: &str) -> Result<String, DriverError> {
        validate_reference(reference)?;
        let digest = self.importer.resolve_digest(reference).await?;
        Ok(cache_alias_with_prefix(&self.prefix, &digest))
    }

    /// Extracts the supervisor binary from `reference` into `cache_dir`, returning the host binary path and image digest.
    pub async fn extract_supervisor_binary(
        &self,
        reference: &str,
        cache_dir: &Path,
    ) -> Result<(PathBuf, String), DriverError> {
        validate_reference(reference)?;
        let digest = self.importer.resolve_digest(reference).await?;
        let clean_digest = digest.strip_prefix("sha256:").unwrap_or(&digest);
        let target_dir = cache_dir.join(clean_digest);
        let binary_path = target_dir.join("openshell-sandbox");

        let extract_lock = {
            let mut locks = self.supervisor_extract_locks.lock().await;
            locks
                .entry(digest.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };

        let _guard = extract_lock.lock().await;

        if is_valid_cached_binary(&binary_path).await {
            tracing::debug!(path = %binary_path.display(), "supervisor binary cache hit");
            return Ok((binary_path, digest));
        }

        self.importer
            .extract_supervisor_binary(reference, cache_dir)
            .await
    }
}

/// Validates that a cached supervisor binary exists as a regular executable
/// file owned by the current process user, rejecting symlinks.
async fn is_valid_cached_binary(path: &Path) -> bool {
    let Ok(metadata) = tokio::fs::symlink_metadata(path).await else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        tracing::warn!(path = %path.display(), "cached supervisor binary is a symlink or not a regular file; ignoring");
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current_uid = unsafe { libc::getuid() };
        if metadata.uid() != current_uid {
            tracing::warn!(
                path = %path.display(),
                owner = metadata.uid(),
                current_uid,
                "cached supervisor binary is not owned by the driver process; ignoring"
            );
            return false;
        }
        if metadata.mode() & 0o111 == 0 {
            tracing::warn!(
                path = %path.display(),
                "cached supervisor binary is not executable; ignoring"
            );
            return false;
        }
    }
    true
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

/// Real implementation of [`OciImporter`] using `skopeo` and `sqfstar`.
pub struct SkopeoImporter {
    lxd: LxdClient,
    skopeo_path: PathBuf,
    sqfstar_path: PathBuf,
    work_dir: PathBuf,
    /// Deadline for each registry call.
    timeout: Duration,
    /// Deadline for each local conversion step, which scales with the image's
    /// size rather than with registry speed.
    convert_timeout: Duration,
    arch_verified: tokio::sync::OnceCell<()>,
}

impl SkopeoImporter {
    pub fn new(
        lxd: LxdClient,
        skopeo_path: Option<PathBuf>,
        sqfstar_path: Option<PathBuf>,
        work_dir: PathBuf,
        timeout: Duration,
        convert_timeout: Duration,
    ) -> Self {
        Self {
            lxd,
            skopeo_path: skopeo_path.unwrap_or_else(|| PathBuf::from("skopeo")),
            sqfstar_path: sqfstar_path.unwrap_or_else(|| PathBuf::from("sqfstar")),
            work_dir,
            timeout,
            convert_timeout,
            arch_verified: tokio::sync::OnceCell::new(),
        }
    }

    async fn verify_lxd_architecture(&self) -> Result<(), DriverError> {
        self.arch_verified
            .get_or_try_init(|| async {
                self.lxd
                    .verify_architecture(host_lxd_arch())
                    .await
                    .map_err(|e| match e {
                        lxd_client::LxdError::Api { message, .. } => {
                            DriverError::ImageImport(message)
                        }
                        other => DriverError::ImageImport(format!(
                            "failed to verify LXD server architecture: {other}"
                        )),
                    })
            })
            .await
            .map(|_| ())
    }

    /// Creates the scratch directory for one conversion inside the configured
    /// work dir, rather than `TMPDIR`/`/tmp`, which is a small tmpfs on most
    /// modern distributions and cannot hold an unpacked sandbox rootfs.
    /// A `skopeo` command with its registry credentials pinned to this
    /// driver's own.
    ///
    /// skopeo otherwise walks an ambient chain of credential files, and on a
    /// host where another user's container runtime has created
    /// `/run/containers` — root-only, as podman makes it — it fails the whole
    /// pull with `permission denied` on a file it was only probing for. The
    /// driver's registry access must not depend on what else the host happens
    /// to run, so it names the file itself.
    ///
    /// An operator who has credentials to supply sets `REGISTRY_AUTH_FILE` (or
    /// `DOCKER_CONFIG`) on the driver, and that is honored: only an unset
    /// variable is filled in.
    fn skopeo(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.skopeo_path);
        // Only `REGISTRY_AUTH_FILE` is honoured as an opt-out. `DOCKER_CONFIG`
        // is not: setting it re-enters containers/image's ambient chain,
        // including the root-only `/run/containers/<uid>/auth.json` that made
        // pulls fail here in the first place.
        if std::env::var_os("REGISTRY_AUTH_FILE").is_none() {
            cmd.env(
                "REGISTRY_AUTH_FILE",
                self.work_dir.join("registry-auth.json"),
            );
        }
        cmd
    }

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

    /// Converts the OCI image at `oci_dir` into the LXD rootfs squashfs at
    /// `squashfs_path`, injecting the init script on the way through.
    ///
    /// The image is flattened in memory into a tar stream that is piped
    /// straight into `sqfstar`: no unpacked rootfs is ever written, and the
    /// OCI layout is left untouched. Because `sqfstar` records modes straight
    /// into the squashfs rather than `chmod`-ing files on the host, an image
    /// that ships setuid binaries converts even under the strictly
    /// confined snap — which is what lets the driver stop mutating the image.
    async fn flatten_rootfs(
        &self,
        oci_dir: &Path,
        squashfs_path: &Path,
        alias: &str,
    ) -> Result<(), DriverError> {
        let oci = oci_dir.to_path_buf();
        let squashfs = squashfs_path.to_path_buf();
        let sqfstar = self.sqfstar_path.clone();
        let timeout = self.convert_timeout;
        let stats =
            tokio::task::spawn_blocking(move || run_sqfstar(&oci, &squashfs, &sqfstar, timeout))
                .await
                .map_err(|e| {
                    DriverError::ImageImport(format!("image conversion panicked: {e}"))
                })??;

        tracing::info!(
            alias = %alias,
            entries = stats.entries,
            xattrs = stats.xattrs,
            "flattened OCI image into a rootfs squashfs"
        );
        Ok(())
    }
}

/// Runs `sqfstar` to build `squashfs_path` from the flattened `oci_dir`,
/// feeding the rootfs tar to its standard input.
///
/// This is blocking by design: it both writes the tar stream and waits on the
/// child, and it is called from a blocking task rather than blocking the async
/// runtime.
fn run_sqfstar(
    oci_dir: &Path,
    squashfs_path: &Path,
    sqfstar_path: &Path,
    timeout: Duration,
) -> Result<crate::oci_flatten::FlattenStats, DriverError> {
    let stderr_path = squashfs_path.with_extension("sqfstar.stderr");
    let stderr_file = std::fs::File::create(&stderr_path).map_err(|e| {
        DriverError::ImageImport(format!("failed to create {}: {e}", stderr_path.display()))
    })?;

    let mut child = std::process::Command::new(sqfstar_path)
        .args([
            // No `-numeric-owner`: sqfstar only gained it in squashfs-tools
            // 4.7, and the snap's core24 base ships 4.6.1. It is not needed —
            // the flattened tar carries numeric uid/gid and no user/group
            // names, so sqfstar stores the numbers as they are either way.
            OsStr::new("-root-uid"),
            OsStr::new("0"),
            OsStr::new("-root-gid"),
            OsStr::new("0"),
            OsStr::new("-root-mode"),
            OsStr::new("0755"),
            OsStr::new("-default-uid"),
            OsStr::new("0"),
            OsStr::new("-default-gid"),
            OsStr::new("0"),
            OsStr::new("-default-mode"),
            OsStr::new("0755"),
            OsStr::new("-mem"),
            OsStr::new(SQFSTAR_MEM),
            squashfs_path.as_os_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|e| DriverError::ImageImport(format!("failed to execute sqfstar: {e}")))?;

    let mut stdin = child.stdin.take().expect("stdin was piped to sqfstar");
    let flat = crate::oci_flatten::flatten_image_to_tar(
        oci_dir,
        INIT_SCRIPT_CONTENTS.as_bytes(),
        &mut stdin,
    );
    // Close the pipe so sqfstar sees EOF even if the write failed.
    drop(stdin);

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(DriverError::ImageImport(format!(
                        "sqfstar timed out building {}",
                        squashfs_path.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(DriverError::ImageImport(format!(
                    "failed to wait for sqfstar: {e}"
                )))
            }
        }
    };

    if !status.success() {
        let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        return Err(DriverError::ImageImport(format!(
            "sqfstar failed building {}: {stderr}",
            squashfs_path.display()
        )));
    }
    flat
}

#[tonic::async_trait]
impl OciImporter for SkopeoImporter {
    async fn resolve_digest(&self, reference: &str) -> Result<String, DriverError> {
        self.verify_lxd_architecture().await?;
        let target = inspect_target(reference);

        // `--raw` returns the top-level manifest document untouched. Two
        // reasons to prefer it over `inspect --format {{.Digest}}`:
        //
        //  * `{{.Digest}}` reports the digest of the *index* even under
        //    `--override-arch`, so every architecture resolved to the same
        //    digest and shared one cache alias.
        //  * a plain `inspect` also paginates the repository's whole tag
        //    list, which costs ~13s on a repo with thousands of tags and is
        //    pure waste when only the digest is wanted.
        let cmd = self
            .skopeo()
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
        self.verify_lxd_architecture().await?;
        let oci_arch = host_oci_arch();
        let lxd_arch = host_lxd_arch();
        let repo = repo_path(reference);
        let copy_source = format!("docker://{repo}@{digest}");

        // Create scratch directory for conversion, on disk rather than tmpfs.
        let temp_dir = self.scratch_dir("openshell-oci-import-")?;
        let temp_path = temp_dir.path();

        let oci_dest = temp_path.join("oci");
        let squashfs_path = temp_path.join("rootfs.squashfs");

        // 1. skopeo copy docker://<repo>@<digest> oci:<temp_path>/oci:img
        let oci_tag_arg = format!("oci:{}:img", oci_dest.display());
        let cmd = self
            .skopeo()
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

        // 2. Flatten the OCI layout into a single rootfs tar and pipe it into
        //    sqfstar. The image itself is not mutated: the layers are read as
        //    they are, merged in memory, and streamed straight into the
        //    squashfs, so no unpacked rootfs is ever written — and no chmod is
        //    issued, which is what lets a strictly confined snap convert an
        //    image whose layers carry setuid binaries.
        self.flatten_rootfs(&oci_dest, &squashfs_path, alias)
            .await?;

        // The OCI copy has served its purpose. Dropping it keeps peak scratch
        // usage to the squashfs being handed to LXD.
        if let Err(e) = tokio::fs::remove_dir_all(&oci_dest).await {
            tracing::debug!(path = %oci_dest.display(), %e, "could not free OCI copy early");
        }

        // 3. Assemble metadata.tar.xz
        // metadata.yaml contains architecture and creation date
        let creation_date = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let metadata_yaml = format!(
            "architecture: \"{lxd_arch}\"\ncreation_date: {creation_date}\nproperties:\n  description: \"OpenShell sandbox {alias}\"\n"
        );

        let metadata_tar_bytes = create_metadata_tar_xz(temp_path, &metadata_yaml).await?;

        // 4. Upload via LxdClient::create_image_from_split + wait_operation
        let op = tokio::time::timeout(
            self.convert_timeout,
            self.lxd.create_image_from_split(
                "metadata.tar.xz",
                &metadata_tar_bytes,
                "rootfs.squashfs",
                &squashfs_path,
            ),
        )
        .await
        .map_err(|_| DriverError::ImageImport("timed out uploading split image to LXD".into()))?
        .map_err(|e| DriverError::ImageImport(format!("LXD split image upload failed: {e}")))?;

        let finished_op =
            tokio::time::timeout(self.convert_timeout, self.lxd.wait_operation(&op.id))
                .await
                .map_err(|_| {
                    DriverError::ImageImport("timed out waiting for image upload operation".into())
                })?
                .map_err(|e| {
                    DriverError::ImageImport(format!("image upload operation failed: {e}"))
                })?;

        let fingerprint = finished_op
            .metadata
            .as_ref()
            .and_then(|m| m.get("fingerprint"))
            .and_then(|f| f.as_str())
            .ok_or_else(|| {
                DriverError::ImageImport("operation response missing image fingerprint".into())
            })?;

        // 5. Bind the alias to the fingerprint
        if let Err(e) = self
            .lxd
            .create_image_alias(
                alias,
                fingerprint,
                Some(&format!("OpenShell image {digest}")),
            )
            .await
        {
            if let Ok(aliases) = self.lxd.get_image_aliases(fingerprint).await {
                if aliases.is_empty() {
                    if let Ok(del_op) = self.lxd.delete_image(fingerprint).await {
                        let _ =
                            tokio::time::timeout(self.timeout, self.lxd.wait_operation(&del_op.id))
                                .await;
                    }
                }
            }
            return Err(DriverError::ImageImport(format!(
                "failed to create image alias {alias}: {e}"
            )));
        }

        Ok(())
    }

    async fn extract_supervisor_binary(
        &self,
        reference: &str,
        cache_dir: &Path,
    ) -> Result<(PathBuf, String), DriverError> {
        self.verify_lxd_architecture().await?;
        validate_reference(reference)?;
        let digest = self.resolve_digest(reference).await?;
        let clean_digest = digest.strip_prefix("sha256:").unwrap_or(&digest);
        let target_dir = cache_dir.join(clean_digest);
        let binary_path = target_dir.join("openshell-sandbox");

        if is_valid_cached_binary(&binary_path).await {
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

        // 1. skopeo copy docker://<repo>@<digest> oci:<temp_path>/oci:img
        let oci_tag_arg = format!("oci:{}:img", oci_dest.display());
        let cmd = self
            .skopeo()
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

        // 2. Pull /openshell-sandbox out of the flattened image without
        //    unpacking a rootfs; it is the top-most layer's copy, and a
        //    symlink or a non-regular file there is rejected.
        {
            let oci = oci_dest.clone();
            let target = temp_target.clone();
            let reference = reference.to_string();
            tokio::task::spawn_blocking(move || {
                crate::oci_flatten::extract_file(&oci, SUPERVISOR_BINARY_NAME, &target)
            })
            .await
            .map_err(|e| {
                DriverError::ImageImport(format!(
                    "supervisor extraction panicked for {reference}: {e}"
                ))
            })??;
        }

        if is_valid_cached_binary(&binary_path).await {
            let _ = tokio::fs::remove_file(&temp_target).await;
            tracing::debug!(path = %binary_path.display(), "supervisor binary already committed; treating as cache hit");
            return Ok((binary_path, digest));
        }

        if let Err(e) = tokio::fs::rename(&temp_target, &binary_path).await {
            if is_valid_cached_binary(&binary_path).await {
                let _ = tokio::fs::remove_file(&temp_target).await;
                tracing::debug!(path = %binary_path.display(), "supervisor binary already committed by another extractor; treating as cache hit");
                return Ok((binary_path, digest));
            }
            return Err(DriverError::ImageImport(format!(
                "failed to commit extracted supervisor binary to {}: {e}",
                binary_path.display()
            )));
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden by design: the revision is spelled out so that bumping
    /// [`CONVERSION_REVISION`] — which invalidates every cached image on every
    /// host — has to be a deliberate edit here too.
    #[test]
    fn cache_alias_golden() {
        let digest_body = "ab".repeat(32);
        let digest = format!("sha256:{digest_body}");
        let alias = cache_alias(&digest);
        assert_eq!(alias, format!("openshell-oci-r12-{digest_body}"));
        assert_eq!(alias.len(), "openshell-oci-r12-".len() + 64);
    }

    #[test]
    fn cache_alias_uniqueness() {
        let d1 = format!("sha256:{}", "01".repeat(32));
        let d2 = format!("sha256:{}", "02".repeat(32));
        assert_ne!(cache_alias(&d1), cache_alias(&d2));
    }

    #[test]
    fn registry_host_follows_the_docker_rule() {
        // A first segment is a registry only when it carries a dot or a port.
        assert_eq!(registry_host("ghcr.io/nvidia/base:latest"), Some("ghcr.io"));
        assert_eq!(
            registry_host("docker://192.168.1.166:5000/supervisor:v1"),
            Some("192.168.1.166:5000")
        );
        assert_eq!(registry_host("localhost:5000/img"), Some("localhost:5000"));
        // Docker Hub paths name no registry.
        assert_eq!(registry_host("ubuntu:24.04"), None);
        assert_eq!(registry_host("library/ubuntu:24.04"), None);
    }

    #[test]
    fn an_empty_allowlist_allows_every_registry() {
        assert!(check_registry_allowed("ghcr.io/nvidia/base:latest", &[]).is_ok());
        assert!(check_registry_allowed("ubuntu:24.04", &[]).is_ok());
    }

    #[test]
    fn a_configured_allowlist_refuses_everything_else() {
        let allowed = vec!["ghcr.io".to_string(), "192.168.1.166:5000".to_string()];

        assert!(check_registry_allowed("ghcr.io/nvidia/base:latest", &allowed).is_ok());
        assert!(check_registry_allowed("GHCR.IO/nvidia/base:latest", &allowed).is_ok());
        assert!(check_registry_allowed("192.168.1.166:5000/supervisor:v1", &allowed).is_ok());

        assert!(check_registry_allowed("evil.example/pwn:latest", &allowed).is_err());
        // A host-less reference resolves to Docker Hub, which was not allowed.
        assert!(check_registry_allowed("ubuntu:24.04", &allowed).is_err());
        // A registry on the same host but another port is another registry.
        assert!(check_registry_allowed("192.168.1.166:5001/x:v1", &allowed).is_err());
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
    fn inspect_target_drops_the_tag_of_a_pinned_reference() {
        let digest = format!("sha256:{}", "ab".repeat(32));
        let cases = [
            (
                format!("ghcr.io/nvidia/openshell/supervisor:0.0.116@{digest}"),
                format!("docker://ghcr.io/nvidia/openshell/supervisor@{digest}"),
            ),
            (
                format!("docker://registry.example.com:5000/app:v1@{digest}"),
                format!("docker://registry.example.com:5000/app@{digest}"),
            ),
            (
                format!("registry.example.com/app@{digest}"),
                format!("docker://registry.example.com/app@{digest}"),
            ),
            (
                "registry.example.com:5000/app:v1".to_string(),
                "docker://registry.example.com:5000/app:v1".to_string(),
            ),
            ("docker://ubuntu".to_string(), "docker://ubuntu".to_string()),
        ];
        for (reference, expected) in cases {
            assert_eq!(inspect_target(&reference), expected, "{reference}");
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
            if is_valid_cached_binary(&binary_path).await {
                return Ok((binary_path, self.digest_to_return.clone()));
            }
            std::fs::create_dir_all(&target_dir).map_err(|e| {
                DriverError::ImageImport(format!("failed to create cache dir: {e}"))
            })?;
            std::fs::write(&binary_path, b"mock-supervisor-binary").map_err(|e| {
                DriverError::ImageImport(format!("failed to write mock binary: {e}"))
            })?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755));
            }
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
        // Revision-agnostic: this test is about cache hit/miss, not about
        // which conversion revision is current.
        let expected_alias = format!("test-oci-r{CONVERSION_REVISION}-{digest_hex}");
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

    #[test]
    fn digest_of_file_refuses_symlink() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test_bin");
        std::fs::write(&file_path, b"hello supervisor").unwrap();
        let link_path = temp_dir.path().join("test_link");
        std::os::unix::fs::symlink(&file_path, &link_path).unwrap();

        let err = digest_of_file(&link_path).unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[tokio::test]
    async fn extract_supervisor_binary_ignores_symlink_in_cache() {
        let digest_hex = "dd".repeat(32);
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

        let _cache =
            ImageCache::with_checker(alias_checker, importer.clone(), "test-oci-".to_string());

        let temp_cache_dir = tempfile::tempdir().unwrap();
        let target_dir = temp_cache_dir.path().join(&digest_hex);
        std::fs::create_dir_all(&target_dir).unwrap();
        let fake_target = temp_cache_dir.path().join("evil_target");
        std::fs::write(&fake_target, b"evil").unwrap();
        let symlink_bin = target_dir.join("openshell-sandbox");
        std::os::unix::fs::symlink(&fake_target, &symlink_bin).unwrap();

        // Cached binary is a symlink: must NOT be treated as a cache hit,
        // but re-extracted by importer.
        assert!(!is_valid_cached_binary(&symlink_bin).await);
    }
}
