// SPDX-License-Identifier: AGPL-3.0-or-later

use std::process::Command;

#[test]
fn help_lists_socket_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_openshell-driver-lxd"))
        .arg("--help")
        .output()
        .expect("failed to run binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--socket"), "stdout was:\n{stdout}");
}
