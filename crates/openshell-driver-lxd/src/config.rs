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

/// Default sandbox image alias.
pub const DEFAULT_SANDBOX_IMAGE: &str = "openshell-sandbox";

/// Default gRPC port the gateway listens on.
pub const DEFAULT_GATEWAY_GRPC_PORT: u16 = 17670;

/// Default deadline, in seconds, for waiting on an LXD operation to complete.
pub const DEFAULT_OPERATION_TIMEOUT_SECS: u64 = 60;

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

    /// LXD image alias every sandbox is created from.
    #[arg(long, default_value = DEFAULT_SANDBOX_IMAGE)]
    pub default_image: String,

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
