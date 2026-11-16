//! The CUDA toolkit: nvcc compiles .cu files, and its runtime links into whatever uses them. nvcc only accepts the
//! host compilers it was released with, so on Windows it gets the newest Visual Studio it accepts (which may be an
//! older one than the build's), found once by trying each and cached.

use crate::flags::{path_text, CompileSpec};
use crate::platform::Os;
use crate::profile::Opt;
use crate::toolchain::{self, Toolchain};
use crate::util::{self, slash};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

// Bump when the cached layout changes
const CACHE_VERSION: u32 = 1;

const PROBE_SOURCE: &str = "__global__ void probe_kernel() {}\nint main() { probe_kernel<<<1, 1>>>(); return 0; }\n";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cuda {
    pub nvcc: PathBuf,
    pub version: String,
    pub include_dir: PathBuf,
    pub lib_dir: PathBuf,

    /// The host compiler nvcc hands C++ to (-ccbin), and the environment it needs.
    pub host_compiler: PathBuf,
    pub env: Vec<(String, String)>,

    // Validation of the cache entry
    cache_version: u32,
    nvcc_mtime: i64,
    toolchain_identity: u64,
}

impl Cuda {
    /// The toolkit to compile .cu files with, for what `toolchain` builds.
    pub fn find(toolchain: &Toolchain) -> Result<Cuda> {
        if toolchain.platform.is_cross() {
            bail!("CUDA files cannot be cross-built (building for {})", toolchain.platform.name());
        }
        if toolchain.targets_windows() && !toolchain.is_msvc_abi() {
            bail!("CUDA on Windows needs the MSVC ABI (nvcc takes no MinGW compiler): build with --toolchain msvc, clang-cl or clang");
        }

        let nvcc =
            find_nvcc().context("this project has CUDA (.cu) files, but nvcc was not found: install the CUDA toolkit, or set CUDA_PATH to it")?;
        let cache_file = util::user_data_dir().join(format!("cuda-{}.json", toolchain.family.name()));
        let nvcc_mtime = util::mtime(&nvcc).unwrap_or(0);
        if let Some(cached) = load_cached(&cache_file, nvcc_mtime, toolchain.identity()) {
            return Ok(cached);
        }

        let root = nvcc
            .parent()
            .and_then(Path::parent)
            .context("nvcc is not in a toolkit's bin folder")?
            .to_path_buf();
        let lib_dir = [root.join("lib").join("x64"), root.join("lib64"), root.join("lib")]
            .into_iter()
            .find(|folder| folder.is_dir())
            .context("the CUDA toolkit has no lib folder")?;

        let (host_compiler, env) = choose_host_compiler(&nvcc, toolchain)?;
        let cuda = Cuda {
            version: nvcc_version(&nvcc).unwrap_or_else(|| "unknown".into()),
            include_dir: root.join("include"),
            lib_dir,
            host_compiler,
            env,
            nvcc,
            cache_version: CACHE_VERSION,
            nvcc_mtime,
            toolchain_identity: toolchain.identity(),
        };

        let _ = util::write_atomic(&cache_file, serde_json::to_string_pretty(&cuda)?.as_bytes());
        Ok(cuda)
    }

    /// The static CUDA runtime and what it needs, for a program using CUDA code.
    pub fn runtime_libs(&self, toolchain: &Toolchain) -> Vec<String> {
        if toolchain.platform.os == Os::Windows {
            vec![slash(&self.lib_dir.join("cudart_static.lib"))]
        } else {
            vec![
                slash(&self.lib_dir.join("libcudart_static.a")),
                "dl".into(),
                "rt".into(),
                "pthread".into(),
            ]
        }
    }

    /// nvcc's settings for one .cu file: the host compiler gets the build's runtime, exceptions and optimization
    /// through -Xcompiler; `gpus` are `cuda-arch` entries. The file arguments come from `file_args`.
    pub fn compile_args(&self, toolchain: &Toolchain, spec: &CompileSpec, gpus: &[String], extra: &[String]) -> Vec<String> {
        let mut args: Vec<String> = vec!["-c".into(), "--use-local-env".into(), "-ccbin".into(), path_text(&self.host_compiler)];

        // C++ standard: nvcc knows up to C++20
        let digits = spec.standard.trim_start_matches("gnu++").trim_start_matches("c++");
        let standard = if matches!(digits, "23" | "26") { "20" } else { digits };
        args.push(format!("-std=c++{standard}"));

        // Host code: the build's runtime and debug information
        let profile = spec.profile;
        let host = |flag: &str| format!("-Xcompiler={flag}");
        if toolchain.is_msvc_abi() {
            let runtime = match (profile.static_runtime, profile.debug_runtime) {
                (false, false) => "/MD",
                (false, true) => "/MDd",
                (true, false) => "/MT",
                (true, true) => "/MTd",
            };
            args.extend([host("/EHsc"), host(runtime), host("/utf-8")]);
            if profile.debug {
                args.push(host("/Z7"));
            }
        } else {
            if profile.debug {
                args.push("-g".into());
            }
            if spec.position_independent {
                args.push(host("-fPIC"));
            }
        }

        // Optimization and assertions
        if spec.opt == Opt::None {
            args.push(if toolchain.is_msvc_abi() { host("/Od") } else { "-O0".into() });
        } else {
            args.push("-O3".into());
        }
        if !profile.assertions {
            args.push("-DNDEBUG".into());
        }

        // GPUs
        for gpu in gpus.iter().flat_map(|gpus| gpus.split(';')).filter(|gpu| !gpu.is_empty()) {
            args.push(gpu_argument(gpu));
        }

        // Definitions, include folders, the target's own options
        args.extend(spec.defines.iter().map(|define| format!("-D{define}")));
        args.extend(spec.includes.iter().map(|folder| format!("-I{}", path_text(folder))));
        for folder in spec.system_includes {
            args.extend(["-isystem".into(), path_text(folder)]);
        }
        args.extend(extra.iter().cloned());

        args
    }

    /// The source, the object and the header list (a make-style depfile).
    pub fn file_args(source: &Path, object: &Path, depfile: &Path) -> Vec<String> {
        vec![
            "-MD".into(),
            "-MF".into(),
            path_text(depfile),
            path_text(source),
            "-o".into(),
            path_text(object),
        ]
    }

    /// Header folders nvcc and its host compiler bring, left out of dependency tracking.
    pub fn system_dirs(&self) -> Vec<String> {
        let mut folders = vec![self.include_dir.clone()];
        if let Some((_, include)) = self.env.iter().find(|(name, _)| name.eq_ignore_ascii_case("INCLUDE")) {
            folders.extend(std::env::split_paths(include).filter(|folder| !folder.as_os_str().is_empty()));
        }

        folders
            .iter()
            .map(|folder| {
                let mut key = util::path_key(&util::normalize(folder));
                if !key.ends_with('/') {
                    key.push('/');
                }
                key
            })
            .collect()
    }
}

/// One `cuda-arch` entry as nvcc takes it, the way CMake reads CUDA_ARCHITECTURES: "86" is code and PTX for sm_86,
/// "86-real" only code, "86-virtual" only PTX; "native", "all", "all-major" and "sm_86" pass through.
fn gpu_argument(gpu: &str) -> String {
    if matches!(gpu, "native" | "all" | "all-major") || gpu.starts_with("sm_") || gpu.starts_with("compute_") {
        return format!("-arch={gpu}");
    }

    let (number, variant) = match gpu.split_once('-') {
        Some((number, variant)) => (number, variant),
        None => (gpu, ""),
    };
    let code = match variant {
        "real" => format!("sm_{number}"),
        "virtual" => format!("compute_{number}"),
        _ => format!("[sm_{number},compute_{number}]"),
    };
    format!("-gencode=arch=compute_{number},code={code}")
}

/// nvcc: from CUDA_PATH, else PATH, else the newest toolkit in the default install folder.
fn find_nvcc() -> Option<PathBuf> {
    let program = if cfg!(windows) { "nvcc.exe" } else { "nvcc" };
    if let Some(root) = std::env::var_os("CUDA_PATH") {
        let candidate = PathBuf::from(root).join("bin").join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if let Some(found) = util::which("nvcc") {
        return Some(found);
    }

    let install_folder = if cfg!(windows) {
        PathBuf::from(r"C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA")
    } else {
        PathBuf::from("/usr/local")
    };
    let mut toolkits: Vec<PathBuf> = std::fs::read_dir(&install_folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("bin").join(program))
        .filter(|candidate| candidate.is_file())
        .collect();
    toolkits.sort_by_key(|candidate| version_key(&candidate.to_string_lossy()));
    toolkits.pop()
}

/// "v12.8" or "cuda-12.8" in a path -> [12, 8], for sorting.
fn version_key(text: &str) -> Vec<u32> {
    let pattern = regex::Regex::new(r"(\d+)\.(\d+)").unwrap();
    pattern
        .captures_iter(text)
        .last()
        .map(|captures| vec![captures[1].parse().unwrap_or(0), captures[2].parse().unwrap_or(0)])
        .unwrap_or_default()
}

fn nvcc_version(nvcc: &Path) -> Option<String> {
    let output = Command::new(nvcc).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let pattern = regex::Regex::new(r"release (\d+\.\d+)").unwrap();
    pattern.captures(&text).map(|captures| captures[1].to_string())
}

/// The host compiler for nvcc. GNU-style toolchains: the build's own. MSVC-style: the build's Visual Studio if nvcc
/// accepts it, else the newest other installed one it accepts.
fn choose_host_compiler(nvcc: &Path, toolchain: &Toolchain) -> Result<(PathBuf, Vec<(String, String)>)> {
    if !toolchain.is_msvc_abi() {
        return Ok((toolchain.cxx.clone(), toolchain.env.clone()));
    }

    // Refused by every one before: asked again only when nvcc, the build's compiler or the installations change
    let visual_studios = crate::vcpkg::all_visual_studios();
    let refused_file = util::user_data_dir().join(format!("cuda-{}-refused.txt", toolchain.family.name()));
    let refused_key = format!(
        "{CACHE_VERSION}|{}|{}|{}|{}",
        slash(nvcc),
        util::mtime(nvcc).unwrap_or(0),
        toolchain.identity(),
        visual_studios.iter().map(|folder| slash(folder)).collect::<Vec<_>>().join(";")
    );
    if std::fs::read_to_string(&refused_file).ok().as_deref() == Some(refused_key.as_str()) {
        bail!(refused_message(nvcc));
    }

    // A kernel to try each compiler on
    let probe_folder = util::user_data_dir().join("cuda-probe");
    std::fs::create_dir_all(&probe_folder)?;
    std::fs::write(probe_folder.join("probe.cu"), PROBE_SOURCE)?;

    // The build's own compiler
    if let Some(cl) = toolchain::find_in_environment(&toolchain.env, "cl.exe") {
        if nvcc_accepts(nvcc, &cl, &toolchain.env, &probe_folder) {
            return Ok((cl, toolchain.env.clone()));
        }
    }

    // Other Visual Studios (capturing each one's environment takes a second)
    let own = toolchain.visual_studio.as_deref().map(util::path_key);
    let others = visual_studios.into_iter().filter(|folder| Some(util::path_key(folder)) != own);
    for folder in others {
        let Ok(env) = toolchain::visual_studio_environment(&folder, toolchain.platform.cpu) else {
            continue;
        };
        let Some(cl) = toolchain::find_in_environment(&env, "cl.exe") else {
            continue;
        };
        if nvcc_accepts(nvcc, &cl, &env, &probe_folder) {
            return Ok((cl, env));
        }
    }

    let _ = util::write_atomic(&refused_file, refused_key.as_bytes());
    bail!(refused_message(nvcc))
}

fn refused_message(nvcc: &Path) -> String {
    format!(
        "nvcc ({}) accepts none of the installed Visual Studio compilers; install the Visual Studio version its CUDA release supports (CUDA 12 and 13: Visual Studio 2022) or a newer CUDA toolkit",
        slash(nvcc)
    )
}

/// Does nvcc compile the probe kernel in `probe_folder` with this host compiler?
fn nvcc_accepts(nvcc: &Path, compiler: &Path, env: &[(String, String)], probe_folder: &Path) -> bool {
    Command::new(nvcc)
        .args(["-c", "--use-local-env", "-ccbin"])
        .arg(compiler)
        .arg(probe_folder.join("probe.cu"))
        .arg("-o")
        .arg(probe_folder.join("probe.obj"))
        .envs(env.iter().cloned())
        .output()
        .is_ok_and(|output| output.status.success())
}

fn load_cached(cache_file: &Path, nvcc_mtime: i64, toolchain_identity: u64) -> Option<Cuda> {
    let cached: Cuda = serde_json::from_str(&std::fs::read_to_string(cache_file).ok()?).ok()?;
    let valid = cached.cache_version == CACHE_VERSION
        && cached.nvcc_mtime == nvcc_mtime
        && cached.toolchain_identity == toolchain_identity
        && cached.host_compiler.is_file();
    valid.then_some(cached)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_arguments() {
        assert_eq!(gpu_argument("native"), "-arch=native");
        assert_eq!(gpu_argument("86"), "-gencode=arch=compute_86,code=[sm_86,compute_86]");
        assert_eq!(gpu_argument("120-real"), "-gencode=arch=compute_120,code=sm_120");
        assert_eq!(gpu_argument("75-virtual"), "-gencode=arch=compute_75,code=compute_75");
    }
}
