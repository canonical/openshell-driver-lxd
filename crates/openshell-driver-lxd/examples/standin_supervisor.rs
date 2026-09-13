// SPDX-License-Identifier: AGPL-3.0-or-later

//! Stand-in for the OpenShell supervisor, used by the driver's integration
//! tests (`tests/driver_lxd`) through `--supervisor-bin`.
//!
//! The real supervisor needs a gateway: without one it gives up and exits
//! about ten seconds after start, so a sandbox cannot be held in a known state
//! long enough to test the driver against it. This binary takes its place as
//! the sandbox's init and does nothing except what a test asks for:
//!
//! - it runs until told to exit, ignoring LXD's graceful shutdown signal the
//!   same way the real supervisor does;
//! - it exits with `N` as soon as a file containing `N` appears at
//!   [`EXIT_FILE`], which a test pushes through the LXD files API to simulate
//!   the supervisor dying on its own;
//! - `ODL_STANDIN_EXIT_ON_START=N` makes it exit with `N` on every start, and
//!   `ODL_STANDIN_EXIT_ON_FIRST_START=N` only on the sandbox's first start.
//!
//! It prints its arguments and `OPENSHELL_*` environment to the console so
//! tests can check what the driver delivered to the supervisor.

use std::path::Path;
use std::process::exit;
use std::time::Duration;

/// A test writes an exit code here to make the stand-in exit.
const EXIT_FILE: &str = "/var/lib/odl-standin/exit";

/// Present once the stand-in has started in this sandbox at least once.
const STARTED_MARKER: &str = "/var/lib/odl-standin/started";

fn env_exit_code(key: &str) -> Option<i32> {
    std::env::var(key).ok()?.trim().parse().ok()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    println!("odl-standin: started args={args:?}");

    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| key.starts_with("OPENSHELL_"))
        .collect();
    env.sort();
    for (key, value) in env {
        println!("odl-standin: env {key}={value}");
    }

    let first_start = !Path::new(STARTED_MARKER).exists();
    if let Some(parent) = Path::new(STARTED_MARKER).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(STARTED_MARKER, b"");

    if let Some(code) = env_exit_code("ODL_STANDIN_EXIT_ON_START") {
        println!("odl-standin: exiting on start with {code}");
        exit(code);
    }
    if first_start {
        if let Some(code) = env_exit_code("ODL_STANDIN_EXIT_ON_FIRST_START") {
            println!("odl-standin: exiting on first start with {code}");
            exit(code);
        }
    }

    loop {
        if let Ok(contents) = std::fs::read_to_string(EXIT_FILE) {
            let code = contents.trim().parse().unwrap_or(1);
            // Remove the request first so a restarted sandbox keeps running.
            let _ = std::fs::remove_file(EXIT_FILE);
            println!("odl-standin: exiting on request with {code}");
            exit(code);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
