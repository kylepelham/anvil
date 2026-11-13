//! Finds compilers. MSVC is located through vswhere and its environment (vcvarsall) is captured once and cached, so
//! no developer prompt is ever needed. Every detected toolchain is cached and re-validated by file time stamps.

use crate::platform::{Abi, Cpu, Os, Platform};
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

// Bump when the cached Toolchain layout changes (it is part of the cache file's name, so Anvils of different
// versions sharing the cache folder do not overwrite each other's detections)
const CACHE_VERSION: u32 = 6;

const PROBE_SOURCE: &str = "int main() { return 0; }\n";

// Names the compiler cache compiles run through, over the project's `launcher` (for CI)
const LAUNCHER_VARIABLE: &str = "ANVIL_LAUNCHER";

// Environment variables a toolchain's programs run with, as (name, value)
type Environment = Vec<(String, String)>;

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

    /// clang: LLVM's linker is installed, so LTO builds link with it (GNU ld would need the LLVMgold plugin).
    pub lld: bool,

    /// clang and clang-cl for the MSVC ABI: the folder of their AddressSanitizer runtime, as the compiler finds it.
    pub sanitizer_runtime_dir: Option<PathBuf>,

    /// Windows programs with the MSVC ABI (cl, clang-cl, clang for *-windows-msvc), not MinGW's.
    msvc_abi: bool,

    // Validation of the cache entry: the compiler's time stamp, plus the default MSVC tools version
    cache_version: u32,
    compiler_mtime: i64,
    vc_tools_version: Option<String>,
}

/// The compiler the CXX (and CC) environment variables name.
struct NamedCompiler {
    family: Family,
    cxx: PathBuf,
    cc: Option<PathBuf>,
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
            // CXX names the program when it is of the requested family
            let family = Family::parse(name)?;
            let named = named_compiler().ok().flatten().filter(|named| named.family == family);
            return Toolchain::find_named(family, platform, named.as_ref())
                .with_context(|| format!("toolchain `{name}`{for_platform} was requested but is not usable"));
        }

        // CXX names a compiler explicitly: that one; one Anvil cannot use is said once, and `auto` picks instead
        match named_compiler() {
            Ok(Some(named)) => match Toolchain::find_named(named.family, platform, Some(&named)) {
                Ok(toolchain) => return Ok(toolchain),
                Err(error) => warn_unusable_cxx(&format!("{error:#}")),
            },
            Ok(None) => {}
            Err(error) => warn_unusable_cxx(&format!("{error:#}")),
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
        Toolchain::find_named(family, platform, None)
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
        let _ = std::fs::remove_dir_all(util::user_data_dir().join("gcc-probe"));
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
        self.platform.os == Os::Windows && self.msvc_abi
    }

    /// Builds programs for Windows.
    pub fn targets_windows(&self) -> bool {
        self.platform.os == Os::Windows
    }

    /// A family for a platform, with CXX's program when it names one; from the cache when it is still valid.
    fn find_named(family: Family, platform: &Platform, named: Option<&NamedCompiler>) -> Result<Toolchain> {
        // CXX=cl names no program of its own: MSVC is found the same way either way
        let named = named.filter(|_| family != Family::Msvc);

        // A Windows ABI the family cannot build (clang builds both)
        let triple = platform.triple.as_deref().unwrap_or_default();
        match (family, platform.abi()) {
            (Family::Msvc | Family::ClangCl, Some(Abi::Gnu)) => {
                bail!("{} builds MSVC-ABI programs, not MinGW ones ({triple}): use gcc or clang", family.name())
            }
            (Family::Gcc, Some(Abi::Msvc)) => {
                bail!("gcc builds MinGW programs, not MSVC-ABI ones ({triple}): use msvc, clang-cl or clang")
            }
            _ => {}
        }

        // One cache file per family, machine and named compiler
        let mut suffix = if platform.is_cross() {
            format!("-{}", platform.name())
        } else {
            String::new()
        };
        if let Some(named) = named {
            let programs = format!("{}|{}", slash(&named.cxx), named.cc.as_deref().map(slash).unwrap_or_default());
            suffix.push_str(&format!("-{:08x}", util::hash_bytes(programs.as_bytes()) as u32));
        }
        let cache_name = format!("toolchain-{}{suffix}.v{CACHE_VERSION}.json", family.name());
        let cache_file = util::user_data_dir().join(cache_name);

        if let Some(cached) = Toolchain::load_cached(&cache_file) {
            return Ok(cached);
        }

        let toolchain = match family {
            Family::Msvc => detect_msvc(platform)?,
            Family::ClangCl => detect_clang_cl(platform, named)?,
            Family::Clang => detect_clang(platform, named)?,
            Family::Gcc => detect_gcc(platform, named)?,
        };

        // A cache that cannot be written only costs the next run a detection
        let _ = util::write_atomic(&cache_file, serde_json::to_string_pretty(&toolchain)?.as_bytes());

        Ok(toolchain)
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

    // Cross and distribution compilers carry a prefix: x86_64-w64-mingw32-g++, x86_64-conda-linux-gnu-c++
    if file_name == "cl" {
        Some(Family::Msvc)
    } else if file_name.starts_with("clang-cl") {
        Some(Family::ClangCl)
    } else if file_name.starts_with("clang") {
        Some(Family::Clang)
    } else if file_name.contains("g++") || file_name.contains("gcc") || file_name == "c++" || file_name.ends_with("-c++") {
        Some(Family::Gcc)
    } else {
        None
    }
}

/// The compiler CXX names (CC naming the C one), when CXX is set: an error when it is not a usable program.
fn named_compiler() -> Result<Option<NamedCompiler>> {
    let Some(text) = std::env::var("CXX").ok().filter(|text| !text.trim().is_empty()) else {
        return Ok(None);
    };
    let program = without_launcher(&text);
    let family = classify_compiler(&program).context("not a compiler Anvil knows (cl, clang-cl, clang++, g++ or c++)")?;

    // cl is Visual Studio's, found as `msvc` finds it (it is on PATH only in a developer prompt)
    if family == Family::Msvc {
        return Ok(Some(NamedCompiler {
            family,
            cxx: PathBuf::from(program),
            cc: None,
        }));
    }
    let cxx = resolve_program(&program).context("no such program (on PATH or as a path)")?;

    // g++ and c++ may be clang (macOS has no gcc behind them)
    let family = if family == Family::Gcc && reports_clang(&cxx) {
        Family::Clang
    } else {
        family
    };

    let cc = match std::env::var("CC").ok().filter(|text| !text.trim().is_empty()) {
        Some(text) => Some(resolve_program(&without_launcher(&text)).with_context(|| format!("CC={text}: no such program (on PATH or as a path)"))?),
        None => None,
    };

    Ok(Some(NamedCompiler { family, cxx, cc }))
}

/// The compiler of a CXX or CC that runs it through a compiler cache ("ccache g++" -> "g++").
fn without_launcher(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    let is_launcher = |word: &&str| {
        let stem = Path::new(word)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        matches!(stem.as_str(), "ccache" | "sccache" | "buildcache" | "distcc" | "icecc")
    };

    words
        .iter()
        .copied()
        .find(|word| !is_launcher(word))
        .or(words.last().copied())
        .unwrap_or_default()
        .to_string()
}

/// Says once that CXX is set to a compiler Anvil cannot use, so it chooses one itself.
fn warn_unusable_cxx(reason: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }

    let cxx = std::env::var("CXX").unwrap_or_default();
    crate::ui::warn(&format!(
        "CXX={cxx} is not used: {reason}. Choosing a compiler automatically (unset CXX, or pass --toolchain)"
    ));
}

/// A program by name (on PATH) or path, made absolute.
fn resolve_program(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    let found = util::which(text).or_else(|| if cfg!(windows) { util::which(&format!("{text}.exe")) } else { None })?;
    let current_dir = std::env::current_dir().ok()?;

    Some(util::absolute(&current_dir, &found))
}

/// The compiler cache compiles run through: ANVIL_LAUNCHER when it is set (empty turns the cache off), else the
/// project's `launcher`. A program that is not there is an error: a build quietly going without its cache is slow.
pub fn find_launcher(configured: Option<&str>, root: &Path) -> Result<Option<PathBuf>> {
    let (text, source) = match std::env::var(LAUNCHER_VARIABLE) {
        Ok(text) => (text, LAUNCHER_VARIABLE),
        Err(_) => match configured {
            Some(text) => (text.to_string(), "[project] launcher in anvil.toml"),
            None => return Ok(None),
        },
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }

    // A path is the project's (relative to its folder); a name is looked up on PATH
    let found = if text.contains(['/', '\\']) {
        Some(util::absolute(root, Path::new(text))).filter(|path| path.is_file())
    } else {
        resolve_program(text)
    };

    match found {
        Some(program) => Ok(Some(program)),
        None => bail!(
            "{source}: no program `{text}` (on PATH or as a path); install it, or set {LAUNCHER_VARIABLE}= (empty) to build without a compiler cache"
        ),
    }
}

/// Warns when the launcher is a ccache too old to cache cl's compiles (before 4.10, as Strawberry Perl's 4.9.1). Its
/// version is asked once per ccache file and remembered.
pub fn check_launcher(launcher: &Path, family: Family) {
    let is_ccache = launcher.file_stem().is_some_and(|stem| stem.eq_ignore_ascii_case("ccache"));
    if !is_ccache || family != Family::Msvc {
        return;
    }

    let Some(version) = launcher_version(launcher) else { return };
    if parse_version_numbers(&version) < (4, 10) {
        crate::ui::warn(&format!(
            "{} is ccache {version}, which caches nothing for cl: use ccache 4.10 or newer (put it first on PATH, or name it in [project] launcher or {LAUNCHER_VARIABLE})",
            slash(launcher)
        ));
    }
}

/// A compiler cache's version (`ccache version 4.9.1` -> `4.9.1`), from a cache file keyed by its path and time stamp.
fn launcher_version(launcher: &Path) -> Option<String> {
    let cache_file = util::user_data_dir().join("launcher-versions.json");
    let key = util::path_key(launcher);
    let mtime = util::mtime(launcher)?;

    let mut known: std::collections::HashMap<String, (i64, String)> = std::fs::read_to_string(&cache_file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    if let Some((known_mtime, version)) = known.get(&key) {
        if *known_mtime == mtime {
            return Some(version.clone());
        }
    }

    let output = Command::new(launcher).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = regex::Regex::new(r"version (\d+(?:\.\d+)*)").unwrap().captures(&text)?[1].to_string();

    // A cache that cannot be written only costs the next run a `--version`
    known.insert(key, (mtime, version.clone()));
    if let Ok(text) = serde_json::to_string_pretty(&known) {
        let _ = util::write_atomic(&cache_file, text.as_bytes());
    }

    Some(version)
}

/// The major and minor numbers of a version (`4.9.1` -> (4, 9)).
fn parse_version_numbers(version: &str) -> (u32, u32) {
    let mut numbers = version.split('.').map(|number| number.parse().unwrap_or(0));

    (numbers.next().unwrap_or(0), numbers.next().unwrap_or(0))
}

/// Does a generic compiler name (c++, and on macOS g++ too) stand for clang (`--version`)?
fn reports_clang(compiler: &Path) -> bool {
    let stem = compiler.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    let generic = matches!(stem.as_str(), "c++" | "cc") || (cfg!(target_os = "macos") && matches!(stem.as_str(), "g++" | "gcc"));
    if !generic {
        return false;
    }

    Command::new(compiler)
        .arg("--version")
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("clang"))
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
    let missing = |program: &str| {
        let folder = visual_studio
            .as_deref()
            .map(|folder| format!(" of {}", slash(folder)))
            .unwrap_or_default();
        format!(
            "{program} not found on the PATH of the Visual Studio environment{folder} for {}; install or repair the \"Desktop development with C++\" workload (MSVC build tools for {}) in the Visual Studio Installer",
            platform.cpu.name(),
            platform.cpu.name()
        )
    };
    let cl = find_in(&search_path, "cl.exe").with_context(|| missing("cl.exe"))?;
    let linker = find_in(&search_path, "link.exe").with_context(|| missing("link.exe"))?;
    let archiver = find_in(&search_path, "lib.exe").with_context(|| missing("lib.exe"))?;
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
        lld: false,
        sanitizer_runtime_dir: None,
        msvc_abi: true,
        cache_version: CACHE_VERSION,
    })
}

fn detect_clang_cl(platform: &Platform, named: Option<&NamedCompiler>) -> Result<Toolchain> {
    if !cfg!(windows) || platform.os != Os::Windows {
        bail!("clang-cl builds Windows programs on Windows only");
    }

    // clang-cl needs Visual Studio's headers and libraries, the target processor's
    let (environment, visual_studio) = msvc_environment(platform.cpu)?;
    let search_path = environment_value(&environment, "PATH").unwrap_or_default();

    let clang_cl = match named {
        Some(named) => named.cxx.clone(),
        None => find_llvm_tool("clang-cl", visual_studio.as_deref())
            .context("clang-cl.exe not found (install the \"C++ Clang tools\" VS component or LLVM)")?,
    };
    let llvm_bin = clang_cl.parent().unwrap().to_path_buf();

    // LLVM's linker and archiver beside it, else Microsoft's
    let missing = |what: &str, llvm: &str, microsoft: &str| {
        format!(
            "no {what}: neither {llvm} beside {} nor {microsoft} on the PATH of the Visual Studio environment; install LLVM (with its {llvm}) or Visual Studio's \"Desktop development with C++\" workload",
            slash(&clang_cl)
        )
    };
    let linker = existing(llvm_bin.join("lld-link.exe"))
        .or_else(|| find_in(&search_path, "link.exe"))
        .with_context(|| missing("linker", "lld-link.exe", "link.exe"))?;
    let archiver = existing(llvm_bin.join("llvm-lib.exe"))
        .or_else(|| find_in(&search_path, "lib.exe"))
        .with_context(|| missing("archiver", "llvm-lib.exe", "lib.exe"))?;
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
    let sanitizer_runtime_dir = clang_sanitizer_runtime_dir(&clang_cl, true, &compile_target_flags, platform.cpu);

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
        lld: llvm_bin.join("lld-link.exe").is_file(),
        sanitizer_runtime_dir,
        msvc_abi: true,
        cache_version: CACHE_VERSION,
    })
}

fn detect_clang(platform: &Platform, named: Option<&NamedCompiler>) -> Result<Toolchain> {
    let visual_studio = if cfg!(windows) { find_visual_studio() } else { None };
    let clang = match named {
        Some(named) => named.cxx.clone(),
        None => find_llvm_tool("clang++", visual_studio.as_deref()).context("clang++ not found")?,
    };
    let bin = clang.parent().unwrap().to_path_buf();

    // Windows programs get the ABI the target names, else clang's own default (MSVC's, or MinGW's for an MSYS2 clang)
    let default_target = clang_default_target(&clang).unwrap_or_default();
    let default_msvc_abi = default_target.contains("msvc") || (default_target.is_empty() && cfg!(windows));
    let msvc_abi = platform.os == Os::Windows
        && match platform.abi() {
            Some(abi) => abi == Abi::Msvc,
            None => default_msvc_abi,
        };

    // The MSVC ABI on Windows needs Visual Studio's environment
    let (environment, system_dirs) = if cfg!(windows) && msvc_abi {
        let (environment, _) = msvc_environment(platform.cpu)?;
        let system_dirs = msvc_system_dirs(&environment);
        (environment, system_dirs)
    } else {
        (Vec::new(), unix_system_dirs(&clang))
    };

    let clang_c = named
        .and_then(|named| named.cc.clone())
        .or_else(|| existing(bin.join(sibling_program(&clang, "clang"))))
        .unwrap_or_else(|| clang.clone());
    let archiver = existing(bin.join(exe("llvm-ar"))).or_else(|| util::which("ar")).with_context(|| {
        format!(
            "no archiver: neither llvm-ar beside {} nor ar on PATH; install LLVM with its tools, or binutils",
            slash(&clang)
        )
    })?;
    let resource_compiler = if platform.os == Os::Windows {
        existing(bin.join(exe("llvm-rc")))
    } else {
        None
    };

    // LLVM's linker, where clang looks for it (beside its real program, or on PATH); macOS's ld64 does LTO itself
    let lld_name = if msvc_abi { "lld-link" } else { "ld.lld" };
    let real_bin = std::fs::canonicalize(&clang).ok().and_then(|real| real.parent().map(Path::to_path_buf));
    let lld = platform.os != Os::Macos && (real_bin.is_some_and(|folder| folder.join(exe(lld_name)).is_file()) || util::which(lld_name).is_some());

    let version = clang_version(&clang, &environment).unwrap_or_else(|| "unknown".into());

    // One clang builds for every processor and system it was built with: --target picks
    let target_flags = if platform.is_cross() || msvc_abi != default_msvc_abi {
        vec![format!("--target={}", platform.clang_triple(msvc_abi))]
    } else {
        Vec::new()
    };

    // MinGW programs need MinGW's headers and libraries, which a clang finds only beside a MinGW of that machine
    if platform.os == Os::Windows && !msvc_abi {
        check_mingw_headers(&clang, &target_flags, platform)?;
    }
    let sanitizer_runtime_dir = if msvc_abi {
        clang_sanitizer_runtime_dir(&clang, false, &target_flags, platform.cpu)
    } else {
        None
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
        lld,
        sanitizer_runtime_dir,
        msvc_abi,
        cache_version: CACHE_VERSION,
    })
}

fn detect_gcc(platform: &Platform, named: Option<&NamedCompiler>) -> Result<Toolchain> {
    let host = Platform::host();

    // The program: CXX's, a cross gcc named for the target (aarch64-linux-gnu-g++), else the plain g++
    let cross_name = format!("{}g++", platform.gnu_prefix());
    let same_machine = platform.cpu == host.cpu && platform.os == host.os;
    let gxx = match named {
        Some(named) => named.cxx.clone(),
        None => match util::which(&cross_name).filter(|_| !same_machine) {
            Some(cross) => cross,
            None => util::which("g++").context("g++ not found on PATH")?,
        },
    };

    // macOS's g++ is Apple's clang under another name
    if named.is_none() && reports_clang(&gxx) {
        bail!(
            "{} is clang (macOS's g++ is Apple clang): use the clang toolchain, or install GCC (Homebrew's g++-15) and name it in CXX",
            slash(&gxx)
        );
    }

    // It builds for its own machine; a 64-bit gcc builds 32-bit programs of its system with -m32, when it can
    let machine = gcc_output(&gxx, "-dumpmachine").with_context(|| format!("running {}", slash(&gxx)))?;
    let builds_for = Platform::parse(&machine).ok();
    let target_flags = match builds_for {
        Some(builds_for) if builds_for.cpu == platform.cpu && builds_for.os == platform.os => Vec::new(),
        Some(builds_for) if builds_for.os == platform.os && builds_for.cpu == Cpu::X64 && platform.cpu == Cpu::X86 => {
            check_m32(&gxx)?;
            vec!["-m32".to_string()]
        }
        _ => bail!(
            "{} builds programs for {machine}, not {}: put the cross compiler for it ({cross_name}) on PATH, or name it in CXX",
            slash(&gxx),
            platform.name()
        ),
    };

    // Its C compiler, archiver and resource compiler beside it, named alike (gcc-12 for g++-12)
    let bin = gxx.parent().unwrap().to_path_buf();
    let gcc = named
        .and_then(|named| named.cc.clone())
        .or_else(|| existing(bin.join(sibling_program(&gxx, "gcc"))))
        .unwrap_or_else(|| gxx.clone());
    let archiver = existing(bin.join(sibling_program(&gxx, "gcc-ar")))
        .or_else(|| existing(bin.join(sibling_program(&gxx, "ar"))))
        .or_else(|| existing(bin.join(exe("ar"))))
        .with_context(|| format!("no archiver (gcc-ar or ar) beside {}", slash(&gxx)))?;
    let resource_compiler = if platform.os == Os::Windows {
        existing(bin.join(sibling_program(&gxx, "windres"))).or_else(|| existing(bin.join(exe("windres"))))
    } else {
        None
    };

    let version = gcc_output(&gxx, "-dumpfullversion").unwrap_or_default();

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
        lld: false,
        sanitizer_runtime_dir: None,
        msvc_abi: false,
        cache_version: CACHE_VERSION,
    })
}

/// A gcc's one-line answer to a query (`-dumpmachine`, `-dumpfullversion`).
fn gcc_output(gxx: &Path, query: &str) -> Result<String> {
    let output = Command::new(gxx).arg(query).output()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || text.is_empty() {
        bail!("{} {query} printed nothing", slash(gxx));
    }

    Ok(text)
}

/// Does this gcc build and link a 32-bit program with -m32 (a multilib gcc)? A working one ends up in the toolchain
/// cache; a failure is probed again next time (installing gcc-multilib does not change g++).
fn check_m32(gxx: &Path) -> Result<()> {
    // A folder of this process's own, so parallel Anvils do not write over each other's probe
    let probe_folder = std::env::temp_dir().join(format!("anvil-probe-{}", std::process::id()));
    std::fs::create_dir_all(&probe_folder)?;
    std::fs::write(probe_folder.join("probe.cpp"), PROBE_SOURCE)?;

    let output = Command::new(gxx)
        .arg("-m32")
        .arg(probe_folder.join("probe.cpp"))
        .arg("-o")
        .arg(probe_folder.join(exe("probe32")))
        .output();
    let _ = std::fs::remove_dir_all(&probe_folder);
    let output = output.with_context(|| format!("running {}", slash(gxx)))?;

    if !output.status.success() {
        // The line saying what failed, not ld's "skipping incompatible" notes before it
        let error = String::from_utf8_lossy(&output.stderr);
        let reason = error
            .lines()
            .find(|line| line.contains("cannot find") || line.contains("error"))
            .or_else(|| error.lines().last())
            .unwrap_or_default()
            .trim();
        bail!(
            "{} cannot build 32-bit programs (-m32 failed: {reason}); install a multilib gcc (gcc-multilib) or an i686 cross compiler",
            slash(gxx)
        );
    }

    Ok(())
}

/// The file name of a program named like `compiler` ("g++-12" -> "gcc-12", "aarch64-linux-gnu-g++" ->
/// "aarch64-linux-gnu-ar", "clang++-18" -> "clang-18").
fn sibling_program(compiler: &Path, tool: &str) -> String {
    let stem = compiler.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();

    let name = ["clang++", "g++", "c++"]
        .iter()
        .find_map(|driver| stem.split_once(driver))
        .map(|(prefix, suffix)| {
            // Binutils carry no version suffix
            let suffix = if tool == "ar" || tool == "windres" { "" } else { suffix };
            format!("{prefix}{tool}{suffix}")
        })
        .unwrap_or_else(|| tool.to_string());

    exe(&name)
}

/// The triple a clang builds for by default (x86_64-pc-windows-msvc, x86_64-w64-windows-gnu).
fn clang_default_target(clang: &Path) -> Option<String> {
    let output = Command::new(clang).arg("-dumpmachine").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !text.is_empty()).then_some(text)
}

/// The folder a clang building for the MSVC ABI takes its AddressSanitizer runtime from (lib/clang/<version>/
/// lib/windows), as it looks for the runtime itself; None when it has none.
fn clang_sanitizer_runtime_dir(clang: &Path, clang_cl: bool, target_flags: &[String], cpu: Cpu) -> Option<PathBuf> {
    let name = format!("clang_rt.asan_dynamic-{}.lib", crate::flags::asan_architecture(cpu));
    let query = format!("{}-print-file-name={name}", if clang_cl { "/clang:" } else { "" });
    let output = Command::new(clang).args(target_flags).arg(query).output().ok()?;

    // An unknown file comes back as its bare name
    let file = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    file.parent().filter(|_| file.is_absolute() && file.is_file()).map(Path::to_path_buf)
}

/// Does a clang building MinGW programs find MinGW's headers? Without them every compile would fail on
/// "'stdio.h' file not found"; better to say so now, and let `auto` pick another toolchain.
fn check_mingw_headers(clang: &Path, target_flags: &[String], platform: &Platform) -> Result<()> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(clang)
        .args(target_flags)
        .args(["-x", "c", "-fsyntax-only", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", slash(clang)))?;
    let _ = child.stdin.take().unwrap().write_all(b"#include <stdio.h>\n");
    let output = child.wait_with_output()?;
    if output.status.success() {
        return Ok(());
    }

    let error = String::from_utf8_lossy(&output.stderr);
    let reason = error.lines().find(|line| line.contains("error")).unwrap_or_default().trim();
    bail!(
        "{} has no MinGW headers and libraries for {} ({reason}); put a MinGW for it on PATH ({}gcc), or use llvm-mingw",
        slash(clang),
        platform.name(),
        platform.gnu_prefix()
    )
}

/// The environment of a Visual Studio developer prompt building for `cpu`: the current one when we are in one for
/// that processor, else vcvarsall's.
fn msvc_environment(cpu: Cpu) -> Result<(Environment, Option<PathBuf>)> {
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

    llvm_folders(visual_studio)
        .into_iter()
        .map(|folder| folder.join(exe(name)))
        .find(|candidate| candidate.is_file())
}

/// An LLVM program Anvil runs itself (clang-format, clang-tidy), found as clang is; else an error naming where it looked.
pub fn find_llvm_program(name: &str) -> Result<PathBuf> {
    let visual_studio = if cfg!(windows) { find_visual_studio() } else { None };
    if let Some(found) = find_llvm_tool(name, visual_studio.as_deref()) {
        return Ok(found);
    }

    let mut places = vec!["PATH".to_string()];
    places.extend(llvm_folders(visual_studio.as_deref()).iter().map(|folder| slash(folder)));
    let install = if cfg!(windows) {
        "Install LLVM, or the \"C++ Clang tools for Windows\" component of Visual Studio"
    } else {
        "Install LLVM"
    };
    bail!("{name} not found (looked on {}). {install}, or put {name} on PATH", places.join(", "))
}

/// The folders LLVM programs are looked for in after PATH: Visual Studio's bundled LLVM, the default LLVM install.
fn llvm_folders(visual_studio: Option<&Path>) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    if let Some(visual_studio) = visual_studio {
        folders.push(visual_studio.join(r"VC\Tools\Llvm\x64\bin"));
    }
    if cfg!(windows) {
        folders.push(PathBuf::from(r"C:\Program Files\LLVM\bin"));
    }

    folders
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
impl Toolchain {
    /// A toolchain for command-line tests: `family` building for `platform`, with made-up programs.
    pub fn for_tests(family: Family, platform: Platform, msvc_abi: bool) -> Toolchain {
        Toolchain {
            family,
            version: "1".into(),
            platform,
            compile_target_flags: Vec::new(),
            link_target_flags: Vec::new(),
            cxx: PathBuf::from("/bin/c++"),
            cc: PathBuf::from("/bin/cc"),
            linker: PathBuf::from("/bin/c++"),
            archiver: PathBuf::from("/bin/ar"),
            resource_compiler: None,
            env: Vec::new(),
            system_dirs: Vec::new(),
            visual_studio: None,
            lld: false,
            sanitizer_runtime_dir: None,
            msvc_abi,
            cache_version: CACHE_VERSION,
            compiler_mtime: 0,
            vc_tools_version: None,
        }
    }
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
        assert_eq!(classify_compiler("x86_64-conda-linux-gnu-c++"), Some(Family::Gcc));
        assert_eq!(classify_compiler("cl.exe"), Some(Family::Msvc));
        assert_eq!(without_launcher("ccache g++"), "g++");
        assert_eq!(without_launcher("/usr/bin/sccache /usr/bin/clang++"), "/usr/bin/clang++");
        assert_eq!(without_launcher(" g++ "), "g++");
    }

    #[test]
    fn reads_launcher_versions() {
        assert_eq!(parse_version_numbers("4.9.1"), (4, 9));
        assert_eq!(parse_version_numbers("4.10"), (4, 10));
        assert!(parse_version_numbers("4.9.1") < (4, 10));
    }

    #[test]
    fn sibling_programs() {
        assert_eq!(sibling_program(Path::new("/usr/bin/g++-12"), "gcc"), exe("gcc-12"));
        assert_eq!(sibling_program(Path::new("/usr/bin/g++-12"), "gcc-ar"), exe("gcc-ar-12"));
        assert_eq!(
            sibling_program(Path::new("/usr/bin/aarch64-linux-gnu-g++"), "ar"),
            exe("aarch64-linux-gnu-ar")
        );
        assert_eq!(sibling_program(Path::new("/usr/bin/clang++-18"), "clang"), exe("clang-18"));
        assert_eq!(sibling_program(Path::new("C:/LLVM/bin/clang++.exe"), "clang"), exe("clang"));
    }
}
