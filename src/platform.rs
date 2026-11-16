//! The machine a build produces programs for: a processor and an operating system, the host's unless a cross build
//! asks for another (`--target arm64`, `--target aarch64-linux-gnu`, or a profile's `target`).

use crate::bail_at;
use crate::located::Place;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cpu {
    X64,
    X86,
    Arm64,
    Arm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    Windows,
    Linux,
    Macos,
}

/// The binary interface of Windows programs: Visual Studio's, or MinGW's (GNU).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abi {
    Msvc,
    Gnu,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub cpu: Cpu,
    pub os: Os,

    /// The GNU-style triple the user named ("aarch64-linux-gnu"), when they named one rather than a processor.
    pub triple: Option<String>,
}

impl Cpu {
    /// Anvil's and vcpkg's name: x64, x86, arm64, arm.
    pub fn name(self) -> &'static str {
        match self {
            Cpu::X64 => "x64",
            Cpu::X86 => "x86",
            Cpu::Arm64 => "arm64",
            Cpu::Arm => "arm",
        }
    }

    /// The first part of a triple: x86_64, i686, aarch64, armv7.
    pub fn triple_name(self) -> &'static str {
        match self {
            Cpu::X64 => "x86_64",
            Cpu::X86 => "i686",
            Cpu::Arm64 => "aarch64",
            Cpu::Arm => "armv7",
        }
    }

    fn parse(text: &str) -> Option<Cpu> {
        Some(match text.to_ascii_lowercase().as_str() {
            "x64" | "x86_64" | "amd64" => Cpu::X64,
            "x86" | "i386" | "i486" | "i586" | "i686" | "win32" => Cpu::X86,
            "arm64" | "aarch64" => Cpu::Arm64,
            "arm" | "armv7" | "armv7a" | "armv7l" | "thumbv7a" => Cpu::Arm,
            _ => return None,
        })
    }
}

impl Platform {
    /// The machine Anvil runs on.
    pub fn host() -> Platform {
        let cpu = if cfg!(target_arch = "aarch64") {
            Cpu::Arm64
        } else if cfg!(target_arch = "x86") {
            Cpu::X86
        } else if cfg!(target_arch = "arm") {
            Cpu::Arm
        } else {
            Cpu::X64
        };
        let os = if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            Os::Linux
        };

        Platform { cpu, os, triple: None }
    }

    /// "arm64", "x86" (the host's operating system), or a triple: "aarch64-linux-gnu", "x86_64-w64-mingw32",
    /// "aarch64-pc-windows-msvc", "arm64-apple-darwin".
    pub fn parse(text: &str) -> Result<Platform> {
        let host = Platform::host();
        if let Some(cpu) = Cpu::parse(text) {
            return Ok(Platform {
                cpu,
                os: host.os,
                triple: None,
            });
        }

        let mut parts = text.split('-');
        let first = parts.next().unwrap_or_default();
        let Some(cpu) = Cpu::parse(first) else {
            bail_at!(
                Place::key("target").value(text),
                "invalid-value",
                "target `{text}`: expected x64, x86, arm64, arm, or a triple such as aarch64-linux-gnu"
            );
        };

        let rest = text.to_ascii_lowercase();
        let os = if rest.contains("windows") || rest.contains("mingw") || rest.contains("win32") {
            Os::Windows
        } else if rest.contains("linux") {
            Os::Linux
        } else if rest.contains("darwin") || rest.contains("apple") || rest.contains("macos") {
            Os::Macos
        } else {
            bail_at!(
                Place::key("target").value(text),
                "invalid-value",
                "target `{text}`: cannot tell its operating system (expected windows, mingw, linux or darwin in it)"
            );
        };

        Ok(Platform {
            cpu,
            os,
            triple: Some(text.to_string()),
        })
    }

    /// Not the machine Anvil runs on: another processor or system, or MinGW programs on Windows (whose default is
    /// the MSVC ABI).
    pub fn is_cross(&self) -> bool {
        let host = Platform::host();
        self.cpu != host.cpu || self.os != host.os || self.other_abi()
    }

    /// The Windows ABI the triple names (`-msvc`, `-mingw32`, `-gnu`), if it names one.
    pub fn abi(&self) -> Option<Abi> {
        let triple = self.triple.as_deref()?.to_ascii_lowercase();
        if self.os != Os::Windows {
            None
        } else if triple.contains("msvc") {
            Some(Abi::Msvc)
        } else if triple.contains("mingw") || triple.contains("gnu") || triple.contains("cygwin") {
            Some(Abi::Gnu)
        } else {
            None
        }
    }

    /// Programs built for this platform start on the host: the same system, and a processor it runs (x64 Windows
    /// runs x86 programs; ARM Windows runs x64 and x86 ones too).
    pub fn runs_on_host(&self) -> bool {
        let host = Platform::host();
        if self.os != host.os {
            return false;
        }

        match (host.cpu, self.cpu) {
            (host_cpu, cpu) if host_cpu == cpu => true,
            (Cpu::X64, Cpu::X86) => host.os == Os::Windows,
            (Cpu::Arm64, Cpu::X64 | Cpu::X86) => host.os == Os::Windows || host.os == Os::Macos,
            _ => false,
        }
    }

    /// The name output folders and messages use: the processor, or the whole triple for another system or ABI.
    pub fn name(&self) -> String {
        match &self.triple {
            Some(triple) if self.os != Platform::host().os || self.other_abi() => triple.clone(),
            _ => self.cpu.name().to_string(),
        }
    }

    /// The triple clang's --target takes: the one the user named, else the usual one for the ABI.
    pub fn clang_triple(&self, msvc_abi: bool) -> String {
        if let Some(triple) = &self.triple {
            return triple.clone();
        }

        let cpu = self.cpu.triple_name();
        match self.os {
            Os::Windows if msvc_abi => format!("{cpu}-pc-windows-msvc"),
            Os::Windows => format!("{cpu}-w64-mingw32"),
            Os::Linux if self.cpu == Cpu::Arm => "armv7-linux-gnueabihf".to_string(),
            Os::Linux => format!("{cpu}-linux-gnu"),
            Os::Macos => format!("{}-apple-darwin", if self.cpu == Cpu::Arm64 { "arm64" } else { cpu }),
        }
    }

    /// The prefix of a GNU cross toolchain's programs ("aarch64-linux-gnu-" for aarch64-linux-gnu-g++).
    pub fn gnu_prefix(&self) -> String {
        format!("{}-", self.clang_triple(false))
    }

    /// MinGW programs asked for on Windows.
    fn other_abi(&self) -> bool {
        Platform::host().os == Os::Windows && self.abi() == Some(Abi::Gnu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        let arm = Platform::parse("arm64").unwrap();
        assert_eq!(arm.cpu, Cpu::Arm64);
        assert_eq!(arm.os, Platform::host().os);

        let linux = Platform::parse("aarch64-linux-gnu").unwrap();
        assert_eq!((linux.cpu, linux.os), (Cpu::Arm64, Os::Linux));
        assert_eq!(linux.gnu_prefix(), "aarch64-linux-gnu-");

        let windows = Platform::parse("i686-pc-windows-msvc").unwrap();
        assert_eq!((windows.cpu, windows.os), (Cpu::X86, Os::Windows));
        assert!(Platform::parse("sparc").is_err());
    }

    #[test]
    fn triples() {
        let x86 = Platform {
            cpu: Cpu::X86,
            os: Os::Windows,
            triple: None,
        };
        assert_eq!(x86.clang_triple(true), "i686-pc-windows-msvc");
        assert_eq!(x86.clang_triple(false), "i686-w64-mingw32");
    }

    #[test]
    fn abis() {
        let mingw = Platform::parse("x86_64-w64-mingw32").unwrap();
        assert_eq!(mingw.abi(), Some(Abi::Gnu));
        assert_eq!(Platform::parse("aarch64-pc-windows-msvc").unwrap().abi(), Some(Abi::Msvc));
        assert_eq!(Platform::parse("aarch64-linux-gnu").unwrap().abi(), None);

        // MinGW programs are another machine's to Windows (MSVC by default), and get their own folder
        if cfg!(all(windows, target_arch = "x86_64")) {
            assert!(mingw.is_cross());
            assert_eq!(mingw.name(), "x86_64-w64-mingw32");
            assert!(!Platform::parse("x86_64-pc-windows-msvc").unwrap().is_cross());
        }
    }
}
