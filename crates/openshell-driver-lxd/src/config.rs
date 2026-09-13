// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

use clap::Parser;

/// Default path for the gRPC Unix domain socket the OpenShell gateway
/// connects to.
pub const DEFAULT_SOCKET: &str = "/var/run/openshell-driver.sock";

/// Default LXD REST API Unix domain socket (snap install).
pub const DEFAULT_LXD_SOCKET: &str = "/var/snap/lxd/common/lxd/unix.socket";

/// Default tracing log level.
pub const DEFAULT_LOG_LEVEL: &str = "info";

/// Default sandbox image: the upstream OpenShell community `base` sandbox
/// image. It is resolved and imported on demand through the OCI import path
/// (like any `template.image`), so no image needs to be pre-built or
/// pre-loaded.
///
/// This is deliberately *not* [`DEFAULT_SUPERVISOR_IMAGE`]: the supervisor
/// image ships the `openshell-sandbox` binary on a minimal BusyBox rootfs and
/// is only ever used as the *source* of that binary. It cannot serve as a
/// sandbox rootfs — BusyBox's `ip` has no `netns` subcommand, so the
/// supervisor's proxy mode fails to isolate and exits at boot.
pub const DEFAULT_SANDBOX_IMAGE: &str = "ghcr.io/nvidia/openshell-community/sandboxes/base:latest";

/// Default LXD project. Re-exported from `lxd_client`.
pub use lxd_client::DEFAULT_PROJECT;

/// Default supervisor OCI image to extract the supervisor binary from.
pub const DEFAULT_SUPERVISOR_IMAGE: &str = "ghcr.io/nvidia/openshell/supervisor:latest";

/// Default host cache directory for extracted supervisor binaries.
pub const DEFAULT_SUPERVISOR_CACHE_DIR: &str = "/var/cache/openshell/lxd-supervisor";

/// Default host scratch directory for OCI image conversion.
///
/// Deliberately disk-backed rather than `TMPDIR`/`/tmp`: converting an image
/// materialises the OCI copy and the unpacked rootfs at the same time, which
/// runs to several gigabytes for a real sandbox image, and `/tmp` is a
/// memory-backed tmpfs on most modern distributions.
pub const DEFAULT_IMAGE_WORK_DIR: &str = "/var/cache/openshell/lxd-image-work";

/// Default deadline, in seconds, for pulling and importing OCI images.
pub const DEFAULT_IMAGE_PULL_TIMEOUT_SECS: u64 = 300;

/// Default `limits.processes` (PID limit) applied to every sandbox container.
///
/// Sandboxes run untrusted agent workloads on a shared host, so an unbounded
/// PID count is a fork-bomb denial of service against co-tenant sandboxes.
/// Set to `0` to leave `pids.max` unlimited.
pub const DEFAULT_MAX_PROCESSES: u32 = 4096;

/// Default gRPC port the gateway listens on.
pub const DEFAULT_GATEWAY_GRPC_PORT: u16 = 17670;

/// Default deadline, in seconds, for waiting on an LXD operation to complete.
pub const DEFAULT_OPERATION_TIMEOUT_SECS: u64 = 60;

/// Default prefix for digest-derived LXD image aliases.
pub const DEFAULT_IMAGE_CACHE_ALIAS_PREFIX: &str = "openshell-oci-";

/// CLI configuration for `openshell-driver-lxd`.
#[derive(Debug, Clone, Parser)]
#[command(name = "openshell-driver-lxd", version, about)]
pub struct Config {
    /// Path to the Unix domain socket the gRPC server listens on.
    #[arg(long, default_value = DEFAULT_SOCKET)]
    pub socket: PathBuf,

    /// Path to the LXD REST API Unix domain socket (local snap installation).
    /// Ignored when --lxd-url is set.
    #[arg(long, default_value = DEFAULT_LXD_SOCKET)]
    pub lxd_socket: PathBuf,

    /// Tracing log level (e.g. "trace", "debug", "info", "warn", "error").
    #[arg(long, default_value = DEFAULT_LOG_LEVEL)]
    pub log_level: String,

    /// OCI image reference every sandbox is created from when the request's
    /// `template.image` is empty. Resolved and imported on demand.
    #[arg(long, default_value = DEFAULT_SANDBOX_IMAGE)]
    pub default_image: String,

    /// LXD project to target for all instances, images, networks, and
    /// operations. The project must already exist; the driver will not create
    /// it.
    #[arg(long, default_value = DEFAULT_PROJECT)]
    pub project: String,

    /// OCI image reference to extract the OpenShell supervisor binary from.
    #[arg(long, default_value = DEFAULT_SUPERVISOR_IMAGE)]
    pub supervisor_image: String,

    /// Optional path to a pre-extracted OpenShell supervisor binary on the host.
    /// When set, skips extracting the binary from the supervisor OCI image.
    #[arg(long)]
    pub supervisor_bin: Option<PathBuf>,

    /// Optional path to a DHCP client binary on the host (e.g. `udhcpc` or `busybox`).
    /// When unset, the driver searches PATH and standard system locations.
    #[arg(long, alias = "udhcpc-path")]
    pub dhcp_client_bin: Option<PathBuf>,

    /// Host cache directory where extracted supervisor binaries are stored,
    /// keyed by content digest.
    #[arg(long, default_value = DEFAULT_SUPERVISOR_CACHE_DIR)]
    pub supervisor_cache_dir: PathBuf,

    /// LXD storage pool where supervisor and DHCP-client custom storage
    /// volumes are created. When unset, each sandbox's own storage pool
    /// (`driver_config.storage_pool`, itself defaulting to `default`) is
    /// used, so the auxiliary volumes always live alongside the rootfs they
    /// are attached to. Set this to pin every auxiliary volume to one pool.
    #[arg(long)]
    pub supervisor_storage_pool: Option<String>,

    /// Host scratch directory for OCI image conversion. Needs room for the
    /// OCI copy plus the unpacked rootfs (several GiB for a real sandbox
    /// image), so it must not be a small tmpfs such as `/tmp`.
    #[arg(long, default_value = DEFAULT_IMAGE_WORK_DIR)]
    pub image_work_dir: PathBuf,

    /// `limits.processes` applied to every sandbox container, bounding the
    /// PID count a sandbox can consume. `0` leaves it unlimited.
    /// Overridable per sandbox via `driver_config.max_processes`.
    #[arg(long, default_value_t = DEFAULT_MAX_PROCESSES)]
    pub default_max_processes: u32,

    /// Deadline, in seconds, for pulling and importing OCI images before failing.
    #[arg(long, default_value_t = DEFAULT_IMAGE_PULL_TIMEOUT_SECS)]
    pub image_pull_timeout_secs: u64,
    /// Prefix for digest-derived LXD image aliases.
    #[arg(long, default_value = DEFAULT_IMAGE_CACHE_ALIAS_PREFIX)]
    pub image_cache_alias_prefix: String,

    /// Optional path override for the skopeo binary.
    #[arg(long)]
    pub skopeo_path: Option<PathBuf>,

    /// Optional path override for the umoci binary.
    #[arg(long)]
    pub umoci_path: Option<PathBuf>,

    /// Optional path override for the mksquashfs binary.
    #[arg(long)]
    pub mksquashfs_path: Option<PathBuf>,

    /// Remote LXD HTTPS endpoint (e.g. https://10.0.0.1:8443).
    /// When set, --lxd-socket is ignored and HTTPS+mTLS is used instead.
    #[arg(long, requires_all = ["lxd_client_cert", "lxd_client_key"])]
    pub lxd_url: Option<String>,

    /// PEM client certificate for mTLS to a remote LXD (requires --lxd-url).
    #[arg(long, requires_all = ["lxd_url", "lxd_client_key"])]
    pub lxd_client_cert: Option<PathBuf>,

    /// PEM client private key for mTLS to a remote LXD (requires --lxd-url).
    #[arg(long, requires_all = ["lxd_url", "lxd_client_cert"])]
    pub lxd_client_key: Option<PathBuf>,

    /// PEM CA certificate to verify the remote LXD server cert.
    /// Omit to use the built-in webpki CA bundle.
    #[arg(long, requires = "lxd_url")]
    pub lxd_server_ca: Option<PathBuf>,

    /// gRPC port the gateway listens on, used to build OPENSHELL_ENDPOINT for
    /// sandboxes. The host is resolved from the sandbox's own LXD bridge
    /// network at create time.
    #[arg(long, default_value_t = DEFAULT_GATEWAY_GRPC_PORT)]
    pub gateway_grpc_port: u16,

    /// Deadline, in seconds, to wait for an LXD operation to complete before
    /// failing the RPC with DeadlineExceeded.
    #[arg(long, default_value_t = DEFAULT_OPERATION_TIMEOUT_SECS)]
    pub operation_timeout_secs: u64,
}
