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

/// CLI configuration for `openshell-driver-lxd`.
#[derive(Debug, Clone, Parser)]
#[command(name = "openshell-driver-lxd", version, about)]
pub struct Config {
    /// Path to the Unix domain socket the gRPC server listens on.
    #[arg(long, default_value = DEFAULT_SOCKET)]
    pub socket: PathBuf,

    /// Path to the LXD REST API Unix domain socket.
    #[arg(long, default_value = DEFAULT_LXD_SOCKET)]
    pub lxd_socket: PathBuf,

    /// Tracing log level (e.g. "trace", "debug", "info", "warn", "error").
    #[arg(long, default_value = DEFAULT_LOG_LEVEL)]
    pub log_level: String,
}
