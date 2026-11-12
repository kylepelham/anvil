//! Finds compilers. MSVC is located through vswhere and its environment (vcvarsall) is captured once and cached, so
//! no developer prompt is ever needed. Every detected toolchain is cached and re-validated by file time stamps.

use crate::platform::{Cpu, Os, Platform};
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

// Bump when the cached Toolchain layout changes
const CACHE_VERSION: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Family {
    Msvc,
    ClangCl,
    Clang,
    Gcc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Toolchain {
    pub family: Family,
    pub version: String,

    /// The machine its programs run on.
    pub platform: Platform,
    /// Flags selecting that machine, for cross builds: compiling (clang's --target, gcc's -m32) and linking.
    pub compile_target_flags: Vec<String>,
    pub link_target_flags: Vec<String>,

    // Programs
    pub cxx: PathBuf,
    pub cc: PathBuf,
    pub linker: PathBuf,
    pub archiver: PathBuf,
    pub resource_compiler: Option<PathBuf>,

    /// Environment the tools need (MSVC's PATH, INCLUDE, LIB, LIBPATH); empty for gcc.
    pub env: Vec<(String, String)>,

    /// Lowercased, forward-slash folder prefixes of the toolchain's own headers, left out of dependency tracking.
    pub system_dirs: Vec<String>,

    /// Visual Studio folder, when the toolchain came from one.
    pub visual_studio: Option<PathBuf>,

    // Validation of the cache entry: the compiler's time stamp, plus the default MSVC tools version
    cache_version: u32,
    compiler_mtime: i64,
    vc_tools_version: Option<String>,
}

impl Family {
    pub fn parse(name: &str) -> Result<Family> {
        Ok(match name {
            "msvc" | "cl" => Family::Msvc,
            "clang-cl" => Family::ClangCl,
            "clang" => Family::Clang,
            "gcc" | "g++" | "mingw" => Family::Gcc,
            other => bail!("unknown toolchain `{other}` (expected msvc, clang-cl, clang or gcc)"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Family::Msvc => "msvc",
            Family::ClangCl => "clang-cl",
            Family::Clang => "clang",
            Family::Gcc => "gcc",
        }
    }

    /// MSVC-style command line (cl, clang-cl).
    pub fn is_msvc_style(self) -> bool {
        matches!(self, Family::Msvc | Family::ClangCl)
    }

    /// The order `auto` tries toolchains in.
    pub fn preference_order() -> &'static [Family] {
        if cfg!(windows) {
            &[Family::Msvc, Family::ClangCl, Family::Gcc, Family::Clang]
        } else {
            &[Family::Clang, Family::Gcc]
        }
    }
}

impl Toolchain {
    /// The assembler: ml64 (armasm64 on ARM) from the MSVC tools for MSVC-style toolchains, the C compiler otherwise.
    pub fn assembler(&self) -> Option<PathBuf> {
        if !self.family.is_msvc_style() {
            return Some(self.cc.clone());
        }

        let name = match self.platform.cpu {
            Cpu::X64 => "ml64.exe",
            Cpu::X86 => "ml.exe",
            Cpu::Arm64 => "armasm64.exe",
            Cpu::Arm => "armasm.exe",
        };
        let beside_compiler = self.cxx.parent().map(|folder| folder.join(name)).filter(|file| file.is_file());
        let tool_path = self
            .env
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
            .map(|(_, value)| value.clone());
        let on_tool_path = || {
            std::env::split_paths(tool_path.as_deref().unwrap_or_default())
                .map(|folder| folder.join(name))
                .find(|file| file.is_file())
        };

        beside_compiler.or_else(on_tool_path)
    }

    /// The toolchain to build for `platform` with: the requested one, else the best installed one.
    pub fn find(requested: Option<&str>, platform: &Platform) -> Result<Toolchain> {
        let for_platform = if platform.is_cross() {
            format!(" for {}", platform.name())
        } else {
            String::new()
        };
        if let Some(name) = requested.filter(|name| *name != "auto") {
            let family = Family::parse(name)?;
            return Toolchain::find_family(family, platform)
                .with_context(|| format!("toolchain `{name}`{for_platform} was requested but is not usable"));
        }

        // CXX names a compiler explicitly
        if let Ok(compiler) = std::env::var("CXX") {
            if let Some(family) = classify_compiler(&compiler) {
                if let Ok(toolchain) = Toolchain::find_family(family, platform) {
                    return Ok(toolchain);
                }
            }
        }

        let mut failures = Vec::new();
        for family in Family::preference_order() {
            match Toolchain::find_family(*family, platform) {
                Ok(toolchain) => return Ok(toolchain),
                Err(error) => failures.push(format!("  {}: {error:#}", family.name())),
            }
        }

        bail!(
            "no C++ compiler found{for_platform}. Install Visual Studio (with \"Desktop development with C++\"), LLVM, or GCC.\n{}",
            failures.join("\n")
        )
    }

    /// One family for one platform, from the cache when it is still valid.
    pub fn find_family(family: Family, platform: &Platform) -> Result<Toolchain> {
        let suffix = if platform.is_cross() {
            format!("-{}", platform.name())
        } else {
            String::new()
        };
        let cache_file = util::user_data_dir().join(format!("toolchain-{}{suffix}.json", family.name()));

        if let Some(cached) = Toolchain::load_cached(&cache_file) {
            return Ok(cached);
        }

        let toolchain = match family {
            Family::Msvc => detect_msvc(platform)?,
            Family::ClangCl => detect_clang_cl(platform)?,
            Family::Clang => detect_clang(platform)?,
            Family::Gcc => detect_gcc(platform)?,
        };

        // A cache that cannot be written only costs the next run a detection
        let _ = util::write_atomic(&cache_file, serde_json::to_string_pretty(&toolchain)?.as_bytes());

        Ok(toolchain)
    }

    /// Every toolchain that can be found, for `anvil toolchains`.
    pub fn find_all() -> Vec<(Family, Result<Toolchain>)> {
        Family::preference_order()
            .iter()
            .map(|family| (*family, Toolchain::find_family(*family, &Platform::host())))
            .collect()
    }

    /// Forgets cached detections, cross toolchains' too (after installing or updating a compiler).
    pub fn clear_cache() {
        for entry in std::fs::read_dir(util::user_data_dir()).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("toolchain-") && name.ends_with(".json") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// A hash of everything about the toolchain that changes what it produces.
    pub fn identity(&self) -> u64 {
        let mut text = format!(
            "{:?}|{}|{}|{}|{}|{:?}|{:?}",
            self.family,
            self.version,
            slash(&self.cxx),
            slash(&self.cc),
            self.platform.name(),
            self.compile_target_flags,
            self.link_target_flags
        );
        for (name, value) in &self.env {
            text.push_str(&format!("|{name}={value}"));
        }

        util::hash_bytes(text.as_bytes())
    }

    /// Windows binaries with the MSVC ABI: .lib archives, .pdb files, the MSVC runtime.
    pub fn is_msvc_abi(&self) -> bool {
        self.platform.os == Os::Windows && self.family != Family::Gcc
    }

    /// Builds programs for Windows.
    pub fn targets_windows(&self) -> bool {
        self.platform.os == Os::Windows
    }

    fn load_cached(cache_file: &Path) -> Option<Toolchain> {
        let text = std::fs::read_to_string(cache_file).ok()?;
        let cached: Toolchain = serde_json::from_str(&text).ok()?;

        // Still the same compiler on disk?
        if cached.cache_version != CACHE_VERSION || util::mtime(&cached.cxx) != Some(cached.compiler_mtime) {
            return None;
        }

        // Still the same default MSVC tools in that Visual Studio?
        if let Some(visual_studio) = &cached.visual_studio {
            if read_vc_tools_version(visual_studio) != cached.vc_tools_version {
                return None;
            }
        }

        Some(cached)
    }
}

/// Which family a compiler name or path belongs to.
pub fn classify_compiler(compiler: &str) -> Option<Family> {
    let file_name = Path::new(compiler).file_stem()?.to_string_lossy().to_lowercase();

    if file_name == "cl" {
        Some(Family::Msvc)
    } else if file_name.starts_with("clang-cl") {
        Some(Family::ClangCl)
    } else if file_name.starts_with("clang") {
        Some(Family::Clang)
    } else if file_name.contains("g++") || file_name.contains("gcc") || file_name == "c++" {
        Some(Family::Gcc)
    } else {
        None
    }
}

/// The newest Visual Studio with the C++ tools, through vswhere (or the developer prompt's VSINSTALLDIR).
pub fn find_visual_studio() -> Option<PathBuf> {
    if let Ok(folder) = std::env::var("VSINSTALLDIR") {
        let folder = PathBuf::from(folder);
        if folder.join("VC").is_dir() {
            return Some(folder);
        }
    }

    let program_files = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into());
    let vswhere = PathBuf::from(program_files).join(r"Microsoft Visual Studio\Installer\vswhere.exe");
    if !vswhere.is_file() {
        return None;
    }

    let output = Command::new(vswhere)
        .args(["-latest", "-products", "*", "-prerelease"])
        .args(["-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64"])
        .args(["-property", "installationPath"])
        .output()
        .ok()?;

    let folder = String::from_utf8_lossy(&output.stdout).lines().next()?.trim().to_string();
    (!folder.is_empty()).then(|| PathBuf::from(folder))
}

fn read_vc_tools_version(visual_studio: &Path) -> Option<String> {
    let version_file = visual_studio.join(r"VC\Auxiliary\Build\Microsoft.VCToolsVersion.default.txt");
    std::fs::read_to_string(version_file).ok().map(|text| text.trim().to_string())
}

fn detect_msvc(platform: &Platform) -> Result<Toolchain> {
    if !cfg!(windows) || platform.os != Os::Windows {
        bail!("MSVC builds Windows programs on Windows only");
    }

    let (environment, visual_studio) = msvc_environment(platform.cpu)?;
    let search_path = environment_value(&environment, "PATH").unwrap_or_default();

    // The tools, from the captured PATH
    let cl = find_in(&search_path, "cl.exe").context("cl.exe not found in the Visual Studio environment")?;
    let linker = find_in(&search_path, "link.exe").context("link.exe not found")?;
    let archiver = find_in(&search_path, "lib.exe").context("lib.exe not found")?;
    let resource_compiler = find_in(&search_path, "rc.exe");

    let version = msvc_version(&cl, &environment).unwrap_or_else(|| "unknown".into());

    Ok(Toolchain {
        family: Family::Msvc,
        version,
        platform: platform.clone(),
        compile_target_flags: Vec::new(),
        link_target_flags: Vec::new(),
        cxx: cl.clone(),
        cc: cl.clone(),
        linker,
        archiver,
        resource_compiler,
        system_dirs: msvc_system_dirs(&environment),
        env: environment,
        compiler_mtime: util::mtime(&cl).unwrap_or(0),
        vc_tools_version: visual_studio.as_deref().and_then(read_vc_tools_version),
        visual_studio,
        cache_version: CACHE_VERSION,
    })
}

fn detect_clang_cl(platform: &Platform) -> Result<Toolchain> {
    if !cfg!(windows) || platform.os != Os::Windows {
        bail!("clang-cl builds Windows programs on Windows only");
    }

    // clang-cl needs Visual Studio's headers and libraries, the target processor's
    let (environment, visual_studio) = msvc_environment(platform.cpu)?;
    let search_path = environment_value(&environment, "PATH").unwrap_or_default();

    let clang_cl = find_llvm_tool("clang-cl", visual_studio.as_deref())
        .context("clang-cl.exe not found (install the \"C++ Clang tools\" VS component or LLVM)")?;
    let llvm_bin = clang_cl.parent().unwrap().to_path_buf();

    // LLVM's linker and archiver beside it, else Microsoft's
    let linker = existing(llvm_bin.join("lld-link.exe"))
        .or_else(|| find_in(&search_path, "link.exe"))
        .context("no linker found")?;
    let archiver = existing(llvm_bin.join("llvm-lib.exe"))
        .or_else(|| find_in(&search_path, "lib.exe"))
        .context("no archiver found")?;
    let resource_compiler = find_in(&search_path, "rc.exe").or_else(|| existing(llvm_bin.join("llvm-rc.exe")));

    let version = clang_version(&clang_cl, &[]).unwrap_or_else(|| "unknown".into());

    let (compile_target_flags, link_target_flags) = if platform.is_cross() {
        (
            vec![format!("--target={}", platform.clang_triple(true))],
            vec![format!("/machine:{}", platform.cpu.name())],
        )
    } else {
        (Vec::new(), Vec::new())
    };

    Ok(Toolchain {
        family: Family::ClangCl,
        version,
        platform: platform.clone(),
        compile_target_flags,
        link_target_flags,
        cxx: clang_cl.clone(),
        cc: clang_cl.clone(),
        linker,
        archiver,
        resource_compiler,
        system_dirs: msvc_system_dirs(&environment),
        env: environment,
        compiler_mtime: util::mtime(&clang_cl).unwrap_or(0),
        vc_tools_version: visual_studio.as_deref().and_then(read_vc_tools_version),
        visual_studio,
        cache_version: CACHE_VERSION,
    })
}

fn detect_clang(platform: &Platform) -> Result<Toolchain> {
    let visual_studio = if cfg!(windows) { find_visual_studio() } else { None };
    let clang = find_llvm_tool("clang++", visual_studio.as_deref()).context("clang++ not found")?;
    let bin = clang.parent().unwrap().to_path_buf();

    // Building Windows programs on Windows, clang targets the MSVC ABI and needs Visual Studio's environment
    let (environment, system_dirs) = if cfg!(windows) && platform.os == Os::Windows {
        let (environment, _) = msvc_environment(platform.cpu)?;
        let system_dirs = msvc_system_dirs(&environment);
        (environment, system_dirs)
    } else {
        (Vec::new(), unix_system_dirs(&clang))
    };

    let clang_c = existing(bin.join(exe("clang"))).unwrap_or_else(|| clang.clone());
    let archiver = existing(bin.join(exe("llvm-ar")))
        .or_else(|| util::which("ar"))
        .context("no archiver (llvm-ar or ar) found")?;
    let resource_compiler = if platform.os == Os::Windows {
        existing(bin.join(exe("llvm-rc")))
    } else {
        None
    };

    let version = clang_version(&clang, &environment).unwrap_or_else(|| "unknown".into());

    // One clang builds for every processor and system it was built with: --target picks
    let target_flags = if platform.is_cross() {
        vec![format!("--target={}", platform.clang_triple(cfg!(windows) && platform.os == Os::Windows))]
    } else {
        Vec::new()
    };

    Ok(Toolchain {
        family: Family::Clang,
        version,
        platform: platform.clone(),
        compile_target_flags: target_flags.clone(),
        link_target_flags: target_flags,
        cxx: clang.clone(),
        cc: clang_c,
        linker: clang.clone(),
        archiver,
        resource_compiler,
        env: environment,
        system_dirs,
        compiler_mtime: util::mtime(&clang).unwrap_or(0),
        vc_tools_version: None,
        visual_studio: None,
        cache_version: CACHE_VERSION,
    })
}

fn detect_gcc(platform: &Platform) -> Result<Toolchain> {
    let host = Platform::host();

    // A cross gcc is its own program (aarch64-linux-gnu-g++); a 32-bit build on the same system is -m32
    let (prefix, target_flags) = if !platform.is_cross() {
        (String::new(), Vec::new())
    } else if util::which(&format!("{}g++", platform.gnu_prefix())).is_some() {
        (platform.gnu_prefix(), Vec::new())
    } else if platform.os == host.os && platform.cpu == Cpu::X86 && host.cpu == Cpu::X64 {
        (String::new(), vec!["-m32".to_string()])
    } else {
        bail!(
            "no {}g++ on PATH (install the cross compiler for {})",
            platform.gnu_prefix(),
            platform.name()
        );
    };

    let gxx = util::which(&format!("{prefix}g++")).with_context(|| format!("{prefix}g++ not found on PATH"))?;
    let bin = gxx.parent().unwrap().to_path_buf();

    let gcc = existing(bin.join(exe(&format!("{prefix}gcc")))).unwrap_or_else(|| gxx.clone());
    let archiver = existing(bin.join(exe(&format!("{prefix}gcc-ar"))))
        .or_else(|| existing(bin.join(exe(&format!("{prefix}ar")))))
        .with_context(|| format!("{prefix}ar not found beside {prefix}g++"))?;
    let resource_compiler = if platform.os == Os::Windows {
        existing(bin.join(exe(&format!("{prefix}windres"))))
    } else {
        None
    };

    // Version
    let output = Command::new(&gxx).arg("-dumpfullversion").output().context("running g++")?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

    Ok(Toolchain {
        family: Family::Gcc,
        version,
        platform: platform.clone(),
        compile_target_flags: target_flags.clone(),
        link_target_flags: target_flags,
        cxx: gxx.clone(),
        cc: gcc,
        linker: gxx.clone(),
        archiver,
        resource_compiler,
        env: Vec::new(),
        system_dirs: unix_system_dirs(&gxx),
        compiler_mtime: util::mtime(&gxx).unwrap_or(0),
        vc_tools_version: None,
        visual_studio: None,
        cache_version: CACHE_VERSION,
    })
}

/// The environment of a Visual Studio developer prompt building for `cpu`: the current one when we are in one for
/// that processor, else vcvarsall's.
fn msvc_environment(cpu: Cpu) -> Result<(Vec<(String, String)>, Option<PathBuf>)> {
    // Already inside a developer prompt for the same processor
    let prompt_cpu = std::env::var("VSCMD_ARG_TGT_ARCH").unwrap_or_else(|_| Platform::host().cpu.name().to_string());
    let in_prompt = std::env::var_os("INCLUDE").is_some() && std::env::var_os("VCToolsInstallDir").is_some();
    if in_prompt && prompt_cpu.eq_ignore_ascii_case(cpu.name()) {
        let environment = ["PATH", "INCLUDE", "LIB", "LIBPATH"]
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|value| (name.to_string(), value)))
            .collect();
        return Ok((environment, find_visual_studio()));
    }

    let visual_studio = find_visual_studio().context("Visual Studio with the C++ tools was not found (vswhere found no installation)")?;
    let vcvarsall = visual_studio.join(r"VC\Auxiliary\Build\vcvarsall.bat");
    if !vcvarsall.is_file() {
        bail!("{} is missing", vcvarsall.display());
    }

    Ok((capture_vcvars(&vcvarsall, cpu)?, Some(visual_studio)))
}

/// Runs vcvarsall.bat for the host and `cpu` (`x64`, or `x64_arm64` for a cross build) and reads back the variables
/// it sets.
#[cfg(windows)]
fn capture_vcvars(vcvarsall: &Path, cpu: Cpu) -> Result<Vec<(String, String)>> {
    use std::os::windows::process::CommandExt;

    let host = Platform::host().cpu;
    let architecture = if host == cpu {
        host.name().to_string()
    } else {
        format!("{}_{}", host.name(), cpu.name())
    };
    let script = format!("\"\"{}\" {architecture} >nul 2>&1 && set\"", vcvarsall.display());

    // raw_arg: cmd.exe has its own quoting rules that Rust's escaping would break
    let output = Command::new("cmd.exe")
        .raw_arg("/d /s /c")
        .raw_arg(&script)
        .output()
        .context("running vcvarsall.bat")?;
    if !output.status.success() {
        bail!("vcvarsall.bat failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let mut environment = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((name, value)) = line.split_once('=') else { continue };

        let wanted = ["PATH", "INCLUDE", "LIB", "LIBPATH"]
            .iter()
            .any(|wanted| wanted.eq_ignore_ascii_case(name));
        if wanted {
            environment.push((name.to_uppercase(), value.to_string()));
        }
    }

    if environment_value(&environment, "INCLUDE").is_none() {
        bail!(
            "vcvarsall.bat {architecture} did not set INCLUDE; are the C++ build tools for {} installed?",
            cpu.name()
        );
    }

    Ok(environment)
}

#[cfg(not(windows))]
fn capture_vcvars(_vcvarsall: &Path, _cpu: Cpu) -> Result<Vec<(String, String)>> {
    bail!("MSVC is Windows only")
}

/// A program on an environment's PATH.
pub fn find_in_environment(environment: &[(String, String)], file_name: &str) -> Option<PathBuf> {
    find_in(&environment_value(environment, "PATH")?, file_name)
}

/// One Visual Studio installation's build environment for `cpu`.
pub fn visual_studio_environment(visual_studio: &Path, cpu: Cpu) -> Result<Vec<(String, String)>> {
    let vcvarsall = visual_studio.join(r"VC\Auxiliary\Build\vcvarsall.bat");
    if !vcvarsall.is_file() {
        bail!("{} is missing", vcvarsall.display());
    }
    capture_vcvars(&vcvarsall, cpu)
}

fn environment_value(environment: &[(String, String)], name: &str) -> Option<String> {
    environment
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

fn find_in(search_path: &str, file_name: &str) -> Option<PathBuf> {
    std::env::split_paths(search_path)
        .map(|folder| folder.join(file_name))
        .find(|candidate| candidate.is_file())
}

fn existing(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// An LLVM program: on PATH, in Visual Studio's bundled LLVM, or in the default LLVM install folder.
fn find_llvm_tool(name: &str, visual_studio: Option<&Path>) -> Option<PathBuf> {
    if let Some(found) = util::which(name) {
        return Some(found);
    }

    let mut folders = Vec::new();
    if let Some(visual_studio) = visual_studio {
        folders.push(visual_studio.join(r"VC\Tools\Llvm\x64\bin"));
    }
    if cfg!(windows) {
        folders.push(PathBuf::from(r"C:\Program Files\LLVM\bin"));
    }

    folders
        .into_iter()
        .map(|folder| folder.join(exe(name)))
        .find(|candidate| candidate.is_file())
}

/// cl prints its version banner on stderr when run with no arguments.
fn msvc_version(cl: &Path, environment: &[(String, String)]) -> Option<String> {
    let output = Command::new(cl).envs(environment.iter().cloned()).output().ok()?;
    let banner = String::from_utf8_lossy(&output.stderr);

    let pattern = regex::Regex::new(r"(\d+\.\d+\.\d+(?:\.\d+)?)").unwrap();
    pattern.captures(&banner).map(|captures| captures[1].to_string())
}

fn clang_version(clang: &Path, environment: &[(String, String)]) -> Option<String> {
    let output = Command::new(clang).arg("--version").envs(environment.iter().cloned()).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);

    let pattern = regex::Regex::new(r"version (\d+\.\d+\.\d+)").unwrap();
    pattern.captures(&text).map(|captures| captures[1].to_string())
}

fn msvc_system_dirs(environment: &[(String, String)]) -> Vec<String> {
    let include = environment_value(environment, "INCLUDE").unwrap_or_default();

    std::env::split_paths(&include)
        .filter(|folder| !folder.as_os_str().is_empty())
        .map(|folder| {
            let mut key = util::path_key(&util::normalize(&folder));
            if !key.ends_with('/') {
                key.push('/');
            }
            key
        })
        .collect()
}

/// The compiler's own install prefix and the system include folders.
fn unix_system_dirs(compiler: &Path) -> Vec<String> {
    let mut dirs = Vec::new();

    if let Some(prefix) = compiler.parent().and_then(|bin| bin.parent()) {
        let mut key = util::path_key(prefix);
        key.push('/');

        // A compiler living in /usr would make every /usr/local header "system"; only use real toolchain prefixes
        if key != "/usr/" && key != "/" {
            dirs.push(key);
        }
    }

    if !cfg!(windows) {
        dirs.push("/usr/include/".into());
        dirs.push("/usr/lib/".into());
    }

    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies() {
        assert_eq!(classify_compiler("cl"), Some(Family::Msvc));
        assert_eq!(classify_compiler("/usr/bin/clang++-19"), Some(Family::Clang));
        assert_eq!(classify_compiler("clang-cl.exe"), Some(Family::ClangCl));
        assert_eq!(classify_compiler("x86_64-w64-mingw32-g++"), Some(Family::Gcc));
    }
}
