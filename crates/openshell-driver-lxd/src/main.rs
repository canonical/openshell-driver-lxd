// SPDX-License-Identifier: AGPL-3.0-or-later

use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};

use clap::Parser;
use computev1::pb::compute_driver_server::ComputeDriverServer;
use lxd_client::{LxdClient, LxdEndpoint, LxdHttpsConfig};
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use tracing::{info, warn};
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

    let lxd = LxdClient::new(endpoint)?;

    let default_image = config.default_image.clone();
    let driver = LxdComputeDriver::new(config, lxd);

    // Best-effort pre-warm of the default sandbox image. The driver pulls the
    // image from the registry on demand, so a missing local image is not an
    // error; pre-warming here just makes the first create fast and surfaces an
    // invalid reference or an unreachable registry early. Any failure is
    // logged and otherwise ignored — the import is retried on first use.
    match driver.ensure_default_image().await {
        Ok(alias) => {
            info!(image = %default_image, %alias, "default sandbox image ready");
        }
        Err(e) => {
            warn!(
                image = %default_image,
                %e,
                "could not pre-warm default sandbox image; it will be imported on first use"
            );
        }
    }

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
