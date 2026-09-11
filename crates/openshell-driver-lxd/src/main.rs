// SPDX-License-Identifier: AGPL-3.0-or-later

use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};

use clap::Parser;
use computev1::pb::compute_driver_server::ComputeDriverServer;
use lxd_client::{LxdClient, LxdEndpoint, LxdHttpsConfig};
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use openshell_driver_lxd::config::Config;
use openshell_driver_lxd::driver::LxdComputeDriver;
use openshell_driver_lxd::grpc::ComputeDriverService;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level)),
        )
        .init();

    if let Some(parent) = config.socket.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    match fs::symlink_metadata(&config.socket) {
        Ok(metadata) => {
            if metadata.file_type().is_socket() {
                fs::remove_file(&config.socket)?;
            } else {
                return Err(format!(
                    "refusing to remove existing non-socket path at {}",
                    config.socket.display()
                )
                .into());
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    let listener = UnixListener::bind(&config.socket)?;
    fs::set_permissions(&config.socket, fs::Permissions::from_mode(0o600))?;

    let endpoint = if let Some(url) = config.lxd_url.clone() {
        let client_cert = config
            .lxd_client_cert
            .clone()
            .ok_or("--lxd-client-cert is required when --lxd-url is set")?;
        let client_key = config
            .lxd_client_key
            .clone()
            .ok_or("--lxd-client-key is required when --lxd-url is set")?;
        LxdEndpoint::Https(LxdHttpsConfig {
            url,
            client_cert,
            client_key,
            server_ca: config.lxd_server_ca.clone(),
        })
    } else {
        LxdEndpoint::UnixSocket(config.lxd_socket.clone())
    };

    info!(socket = %config.socket.display(), "Starting OpenShell LXD compute driver");

    let lxd = LxdClient::new(endpoint)?.with_project(config.project.clone());

    // Fail fast with a clear diagnostic if the configured project does not
    // exist. This gate must run before image_alias_exists because every
    // subsequent request (including that one) is decorated with
    // project=<config.project>; a missing project would otherwise make the
    // image-alias check return a 404-driven Ok(false) and emit the wrong
    // diagnostic. Only the check itself failing (e.g. LXD not reachable yet)
    // is non-fatal here — that failure mode is already surfaced clearly
    // wherever it's next hit.
    match lxd.project_exists(&config.project).await {
        Ok(false) => {
            error!(
                project = %config.project,
                "configured LXD project does not exist; create it before starting the driver"
            );
            std::process::exit(1);
        }
        Ok(true) => {}
        Err(e) => {
            warn!(
                project = %config.project,
                %e,
                "could not verify configured LXD project exists; continuing anyway"
            );
        }
    }

    // Fail fast with a clear diagnostic rather than letting the first
    // create_sandbox call surface an opaque LXD 404. Only the check itself
    // failing (e.g. LXD not reachable yet) is non-fatal here — that failure
    // mode is already surfaced clearly wherever it's next hit.
    match lxd.image_alias_exists(&config.default_image).await {
        Ok(false) => {
            error!(
                image = %config.default_image,
                "default sandbox image alias not found in LXD; run `make sandbox-image` \
                 (or import it under this alias) before starting the driver"
            );
            std::process::exit(1);
        }
        Ok(true) => {}
        Err(e) => {
            warn!(
                image = %config.default_image,
                %e,
                "could not verify default sandbox image alias exists; continuing anyway"
            );
        }
    }

    let driver = LxdComputeDriver::new(config, lxd);
    let service = ComputeDriverService::new(driver);

    Server::builder()
        .add_service(ComputeDriverServer::new(service))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async {
            tokio::signal::ctrl_c().await.ok();
            info!("Received shutdown signal, draining in-flight requests");
        })
        .await?;

    Ok(())
}
