//! Platform detection: from `uname(2)` machine, never from Android ABI lists.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

pub fn arch_from_machine(machine: &str) -> Result<&'static str> {
    match machine {
        "aarch64" | "arm64" | "aarch64_be" => Ok("arm64"),
        "x86_64" | "amd64" => Ok("amd64"),
        other => Err(Error::invalid(format!("unsupported machine architecture '{other}'"))),
    }
}

impl Platform {
    pub fn from_machine(machine: &str) -> Result<Platform> {
        Ok(Platform { os: "linux".into(), arch: arch_from_machine(machine)?.into() })
    }

    pub fn detect() -> Result<Platform> {
        let u = rustix::system::uname();
        let machine = u.machine().to_string_lossy().into_owned();
        Self::from_machine(&machine)
    }

    /// Parse `os/arch` (e.g. `linux/arm64`); `linux/aarch64` style aliases are accepted.
    pub fn parse(s: &str) -> Result<Platform> {
        let (os, arch) = s
            .split_once('/')
            .ok_or_else(|| Error::invalid(format!("platform '{s}' must look like os/arch")))?;
        if os != "linux" {
            return Err(Error::invalid(format!("unsupported os '{os}' (only linux)")));
        }
        Ok(Platform { os: os.into(), arch: arch_from_machine(arch)?.into() })
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.os, self.arch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_mapping() {
        assert_eq!(Platform::from_machine("aarch64").unwrap().to_string(), "linux/arm64");
        assert_eq!(Platform::from_machine("x86_64").unwrap().to_string(), "linux/amd64");
        assert!(Platform::from_machine("riscv64").is_err());
        assert!(Platform::from_machine("armv7l").is_err());
    }

    #[test]
    fn override_parse() {
        assert_eq!(Platform::parse("linux/arm64").unwrap().arch, "arm64");
        assert_eq!(Platform::parse("linux/x86_64").unwrap().arch, "amd64");
        assert!(Platform::parse("arm64").is_err());
        assert!(Platform::parse("windows/amd64").is_err());
    }

    #[test]
    fn detect_works_here() {
        let p = Platform::detect().unwrap();
        assert!(p.arch == "amd64" || p.arch == "arm64");
    }
}
