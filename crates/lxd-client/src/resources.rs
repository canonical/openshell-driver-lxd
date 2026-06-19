// SPDX-License-Identifier: AGPL-3.0-or-later

//! Conversion from Kubernetes-style resource quantity strings
//! (`DriverResourceRequirements`) to LXD's `limits.cpu`/`limits.memory`
//! config value formats.

use crate::error::LxdError;

/// Converts a CPU quantity (`"500m"`, `"2"`) into LXD's `limits.cpu` value.
///
/// LXD cannot express fractional cores, so a fractional result is rounded
/// up to the nearest whole core (and logged at `WARN`).
pub fn cpu_limit_to_lxd(quantity: &str) -> Result<String, LxdError> {
    let cores: f64 = if let Some(milli) = quantity.strip_suffix('m') {
        let milli: f64 = milli
            .parse()
            .map_err(|_| invalid(quantity, "not a valid millicore value"))?;
        milli / 1000.0
    } else {
        quantity
            .parse()
            .map_err(|_| invalid(quantity, "not a valid core count"))?
    };

    if cores <= 0.0 {
        return Err(invalid(quantity, "must be greater than zero"));
    }

    let whole_cores = (cores.ceil() as u64).max(1);

    if whole_cores as f64 > cores {
        tracing::warn!(
            quantity,
            rounded_to = whole_cores,
            "lxd-client: rounding fractional CPU quantity up to a whole core (LXD cannot express fractional cores)"
        );
    }

    Ok(whole_cores.to_string())
}

/// Converts a memory quantity (`"512Mi"`, `"4Gi"`) into LXD's
/// `limits.memory` value by appending LXD's IEC `B` suffix
/// (`"512Mi"` → `"512MiB"`).
pub fn memory_limit_to_lxd(quantity: &str) -> Result<String, LxdError> {
    const SUFFIXES: [&str; 4] = ["Ki", "Mi", "Gi", "Ti"];

    for suffix in SUFFIXES {
        if let Some(digits) = quantity.strip_suffix(suffix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                return Err(invalid(
                    quantity,
                    "expected a numeric prefix before the suffix",
                ));
            }
            return Ok(format!("{quantity}B"));
        }
    }

    Err(invalid(
        quantity,
        "expected a Ki/Mi/Gi/Ti suffix (e.g. \"512Mi\", \"4Gi\")",
    ))
}

fn invalid(quantity: &str, reason: &str) -> LxdError {
    LxdError::InvalidQuantity {
        quantity: quantity.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_whole_core_passes_through() {
        assert_eq!(cpu_limit_to_lxd("2").unwrap(), "2");
    }

    #[test]
    fn cpu_millicores_round_up() {
        assert_eq!(cpu_limit_to_lxd("500m").unwrap(), "1");
        assert_eq!(cpu_limit_to_lxd("1500m").unwrap(), "2");
    }

    #[test]
    fn cpu_rejects_zero() {
        assert!(cpu_limit_to_lxd("0m").is_err());
    }

    #[test]
    fn cpu_rejects_garbage() {
        assert!(cpu_limit_to_lxd("not-a-number").is_err());
    }

    #[test]
    fn memory_converts_mi_and_gi() {
        assert_eq!(memory_limit_to_lxd("512Mi").unwrap(), "512MiB");
        assert_eq!(memory_limit_to_lxd("4Gi").unwrap(), "4GiB");
    }

    #[test]
    fn memory_rejects_unrecognized_suffix() {
        assert!(memory_limit_to_lxd("512M").is_err());
        assert!(memory_limit_to_lxd("512").is_err());
    }
}
