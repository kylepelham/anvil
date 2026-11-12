//! Turns Anvil's compiler-neutral settings into each toolchain's command lines and file names.

use crate::platform::Os;
use crate::profile::{Opt, Profile};
use crate::toolchain::{Family, Toolchain};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

// The Win32 libraries every Windows program may use, linked by default as Visual Studio and CMake do; unused ones
// cost nothing
const WINDOWS_DEFAULT_LIBS: &[&str] = &[
    "kernel32", "user32", "gdi32", "winspool", "shell32", "ole32", "oleaut32", "uuid", "comdlg32", "advapi32",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    C,
    Cxx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Warnings {
    Off,
    Default,
    High,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Avx,
    Avx2,
    Avx512,
    Native,
}

/// How a compiler reports the headers a translation unit read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepsFormat {
    None,
    /// A make-style depfile (gcc, clang, and run steps).
    Make(PathBuf),
    /// cl's /sourceDependencies JSON.
    MsvcJson(PathBuf),
    /// "Note: including file:" lines on stdout (clang-cl).
    ShowIncludes,
}

/// Everything that decides how one source file compiles.
pub struct CompileSpec<'a> {
    pub language: Language,
    pub standard: &'a str,
    pub profile: &'a Profile,
    pub opt: Opt,
    pub warnings: Warnings,
    pub werror: bool,
    pub arch: Option<Arch>,
    pub defines: &'a [String],
    pub includes: &'a [PathBuf],
    pub system_includes: &'a [PathBuf],
    pub flags: &'a [String],
    pub position_independent: bool,
}

/// Everything that decides how one binary links.
pub struct LinkSpec<'a> {
    pub profile: &'a Profile,
    pub shared: bool,
    pub gui: bool,
    /// A program that exports symbols to the modules it loads.
    pub exports: bool,
    pub objects: &'a [PathBuf],
    pub libs: &'a [String],
    /// Static libraries linked whole.
    pub whole: &'a [String],
    pub lib_dirs: &'a [PathBuf],
    pub flags: &'a [String],
    pub output: &'a Path,
    pub import_lib: Option<&'a Path>,
}

impl Warnings {
    pub fn parse(text: &str) -> Result<Warnings> {
        Ok(match text {
            "off" | "none" => Warnings::Off,
            "default" => Warnings::Default,
            "high" | "extra" => Warnings::High,
            "all" | "pedantic" => Warnings::All,
            other => bail!("warnings = \"{other}\": expected off, default, high or all"),
        })
    }
}

impl Arch {
    pub fn parse(text: &str) -> Result<Arch> {
        Ok(match text {
            "avx" => Arch::Avx,
            "avx2" => Arch::Avx2,
            "avx512" => Arch::Avx512,
            "native" => Arch::Native,
            other => bail!("arch = \"{other}\": expected avx, avx2, avx512 or native"),
        })
    }
}

/// "20", "c++20", "cxx20", "cpp20" -> "c++20"; "gnu++20" stays; C: "17", "c17" -> "c17".
pub fn normalize_standard(text: &str, language: Language) -> Result<String> {
    let lowered = text.to_ascii_lowercase();
    let digits = lowered
        .trim_start_matches("c++")
        .trim_start_matches("cxx")
        .trim_start_matches("cpp")
        .trim_start_matches("gnu++")
        .trim_start_matches('c');

    let valid = match language {
        Language::Cxx => ["11", "14", "17", "20", "23", "26"].contains(&digits),
        Language::C => ["89", "90", "99", "11", "17", "18", "23"].contains(&digits),
    };
    if !valid {
        bail!("unsupported language standard \"{text}\"");
    }

    Ok(match language {
        Language::Cxx if lowered.starts_with("gnu") => format!("gnu++{digits}"),
        Language::Cxx => format!("c++{digits}"),
        Language::C => format!("c{digits}"),
    })
}

impl Toolchain {
    // File names

    pub fn object_extension(&self) -> &'static str {
        if self.family.is_msvc_style() {
            "obj"
        } else {
            "o"
        }
    }

    /// File name prefix and extension of a program, a static library and a shared library.
    pub fn executable_affixes(&self) -> (&'static str, &'static str) {
        if self.targets_windows() {
            ("", ".exe")
        } else {
            ("", "")
        }
    }

    pub fn static_lib_affixes(&self) -> (&'static str, &'static str) {
        if self.is_msvc_abi() {
            ("", ".lib")
        } else {
            ("lib", ".a")
        }
    }

    pub fn shared_lib_affixes(&self) -> (&'static str, &'static str) {
        if self.targets_windows() {
            ("", ".dll")
        } else if self.platform.os == Os::Macos {
            ("lib", ".dylib")
        } else {
            ("lib", ".so")
        }
    }

    /// The shared library, and on Windows the import library dependents link against.
    pub fn shared_lib_names(&self, name: &str) -> (String, Option<String>) {
        if self.is_msvc_abi() {
            (format!("{name}.dll"), Some(format!("{name}.lib")))
        } else if self.targets_windows() {
            (format!("{name}.dll"), Some(format!("lib{name}.dll.a")))
        } else if self.platform.os == Os::Macos {
            (format!("lib{name}.dylib"), None)
        } else {
            (format!("lib{name}.so"), None)
        }
    }

    // Compiling

    /// The compiler program for a language.
    pub fn compiler(&self, language: Language) -> &Path {
        match language {
            Language::C => &self.cc,
            Language::Cxx => &self.cxx,
        }
    }

    /// Arguments for compiling `source` to `object`, and where the header list will come from.
    pub fn compile_args(&self, spec: &CompileSpec, source: &Path, object: &Path, deps_file: &Path) -> (Vec<String>, DepsFormat) {
        let mut args = self.compile_flags(spec);

        let deps = match self.family {
            Family::Msvc => {
                args.push("/sourceDependencies".into());
                args.push(path_text(deps_file));
                DepsFormat::MsvcJson(deps_file.to_path_buf())
            }
            Family::ClangCl => {
                args.push("/showIncludes".into());
                DepsFormat::ShowIncludes
            }
            Family::Clang | Family::Gcc => {
                args.extend(["-MD".into(), "-MF".into(), path_text(deps_file)]);
                DepsFormat::Make(deps_file.to_path_buf())
            }
        };

        // Source and output
        if self.family.is_msvc_style() {
            args.push("/c".into());
            args.push(format!("/Fo{}", path_text(object)));
            args.push(if spec.language == Language::C { "/Tc" } else { "/Tp" }.to_string() + &path_text(source));
        } else {
            args.extend(["-c".into(), path_text(source), "-o".into(), path_text(object)]);
        }

        (args, deps)
    }

    /// Precompiled header: the extra arguments that create it (`create`) or use it.
    pub fn pch_args(&self, header: &Path, pch_file: &Path, create: bool) -> Vec<String> {
        let header = path_text(header);
        let pch_file_text = path_text(pch_file);

        match self.family {
            Family::Msvc | Family::ClangCl => {
                let mode = if create { "/Yc" } else { "/Yu" };
                vec![format!("{mode}{header}"), format!("/Fp{pch_file_text}"), format!("/FI{header}")]
            }
            Family::Clang => {
                if create {
                    Vec::new()
                } else {
                    vec!["-include-pch".into(), pch_file_text]
                }
            }
            // gcc finds <stub>.gch beside the stub header it is told to include
            Family::Gcc => {
                if create {
                    Vec::new()
                } else {
                    let stub = pch_file.with_extension("");
                    vec!["-include".into(), path_text(&stub)]
                }
            }
        }
    }

    /// Arguments compiling a header into a precompiled header (gcc and clang).
    pub fn pch_header_args(&self, spec: &CompileSpec, header: &Path, pch_file: &Path, deps_file: &Path) -> Vec<String> {
        let mut args = self.compile_flags(spec);
        args.extend(self.pch_args(header, pch_file, true));

        let header_language = if spec.language == Language::C { "c-header" } else { "c++-header" };
        args.extend(["-MD".into(), "-MF".into(), path_text(deps_file)]);
        args.extend(["-x".into(), header_language.into(), path_text(header), "-o".into(), path_text(pch_file)]);

        args
    }

    /// Every compile flag except the source, output and dependency arguments.
    pub fn compile_flags(&self, spec: &CompileSpec) -> Vec<String> {
        let mut flags = self.compile_target_flags.clone();
        if self.family.is_msvc_style() {
            flags.extend(self.msvc_compile_flags(spec));
        } else {
            flags.extend(self.gnu_compile_flags(spec));
        }
        flags
    }

    // Linking

    /// Arguments for linking an executable or shared library (program: `self.linker`).
    pub fn link_args(&self, spec: &LinkSpec) -> Vec<String> {
        let mut args = self.link_target_flags.clone();
        if self.family.is_msvc_style() {
            args.extend(self.msvc_link_args(spec));
        } else {
            args.extend(self.gnu_link_args(spec));
        }
        args
    }

    /// Arguments for creating a static library (program: `self.archiver`).
    pub fn archive_args(&self, objects: &[PathBuf], output: &Path, profile: &Profile) -> Vec<String> {
        let mut args = Vec::new();

        if self.archiver_is_lib_exe() {
            args.push("/nologo".into());
            if profile.lto && self.family == Family::Msvc {
                args.push("/LTCG".into());
            }
            args.push(format!("/OUT:{}", path_text(output)));
        } else {
            args.push("rcs".into());
            args.push(path_text(output));
        }

        args.extend(objects.iter().map(|object| path_text(object)));
        args
    }

    /// lib.exe and llvm-lib take /OUT:, ar and llvm-ar take `rcs`.
    pub fn archiver_is_lib_exe(&self) -> bool {
        let name = self
            .archiver
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        name == "lib" || name == "llvm-lib"
    }

    /// Arguments for compiling a Windows resource script (program: `self.resource_compiler`).
    pub fn resource_args(&self, source: &Path, output: &Path, includes: &[PathBuf], defines: &[String]) -> Vec<String> {
        let mut args = Vec::new();

        if self.family == Family::Gcc {
            args.extend(["-O".into(), "coff".into(), "-i".into(), path_text(source), "-o".into(), path_text(output)]);
            args.extend(includes.iter().map(|folder| format!("-I{}", path_text(folder))));
            args.extend(defines.iter().map(|define| format!("-D{define}")));
        } else {
            args.push("/nologo".into());
            args.extend(includes.iter().map(|folder| format!("/I{}", path_text(folder))));
            args.extend(defines.iter().map(|define| format!("/D{define}")));
            args.push(format!("/fo{}", path_text(output)));
            args.push(path_text(source));
        }

        args
    }

    /// Arguments for assembling one file (program: `self.assembler()`).
    pub fn assemble_args(&self, source: &Path, output: &Path, includes: &[PathBuf], defines: &[String]) -> Vec<String> {
        let mut args = Vec::new();

        if self.family.is_msvc_style() {
            args.extend(["/nologo".into(), "/c".into()]);
            args.extend(includes.iter().map(|folder| format!("/I{}", path_text(folder))));
            args.extend(defines.iter().map(|define| format!("/D{define}")));
            args.push(format!("/Fo{}", path_text(output)));
            args.push(path_text(source));
        } else {
            args.push("-c".into());
            args.extend(includes.iter().map(|folder| format!("-I{}", path_text(folder))));
            args.extend(defines.iter().map(|define| format!("-D{define}")));
            args.extend(["-o".into(), path_text(output), path_text(source)]);
        }

        args
    }

    /// The resource compiler's output name for a script.
    pub fn resource_output_extension(&self) -> &'static str {
        if self.family == Family::Gcc {
            "res.o"
        } else {
            "res"
        }
    }

    fn msvc_compile_flags(&self, spec: &CompileSpec) -> Vec<String> {
        let profile = spec.profile;
        let clang_cl = self.family == Family::ClangCl;
        let mut flags: Vec<String> = vec!["/nologo".into(), "/utf-8".into(), "/bigobj".into()];

        // Language
        if spec.language == Language::Cxx {
            flags.extend(["/EHsc".into(), "/permissive-".into(), "/Zc:__cplusplus".into()]);
        }
        flags.extend(msvc_standard_flag(spec.standard, clang_cl));

        // Optimization and debug info
        flags.push(
            match spec.opt {
                Opt::None => "/Od",
                Opt::Some | Opt::Size => "/O1",
                Opt::Fast | Opt::Fastest => "/O2",
                Opt::MinSize => "/O1",
            }
            .into(),
        );
        if spec.opt != Opt::None {
            flags.extend(["/Gy".into(), "/Gw".into()]);
        }
        if profile.debug {
            flags.push("/Z7".into());
        }
        if spec.opt == Opt::None && profile.debug_runtime && profile.sanitize.is_empty() && !clang_cl {
            flags.push("/RTC1".into());
        }

        // Runtime library
        let runtime = match (profile.static_runtime, profile.debug_runtime) {
            (false, false) => "/MD",
            (false, true) => "/MDd",
            (true, false) => "/MT",
            (true, true) => "/MTd",
        };
        flags.push(runtime.into());
        if !profile.assertions {
            flags.push("/DNDEBUG".into());
        }

        // Link-time optimization and sanitizers
        if profile.lto {
            flags.push(if clang_cl { "-flto=thin" } else { "/GL" }.into());
        }
        for sanitizer in &profile.sanitize {
            flags.push(format!("/fsanitize={sanitizer}"));
        }

        // Warnings
        flags.push(
            match spec.warnings {
                Warnings::Off => "/W0",
                Warnings::Default => "/W3",
                Warnings::High | Warnings::All => "/W4",
            }
            .into(),
        );
        if spec.werror {
            flags.push("/WX".into());
        }

        // Instruction set
        match spec.arch {
            Some(Arch::Avx) => flags.push("/arch:AVX".into()),
            Some(Arch::Avx2) => flags.push("/arch:AVX2".into()),
            Some(Arch::Avx512) => flags.push("/arch:AVX512".into()),
            Some(Arch::Native) if clang_cl => flags.push("/clang:-march=native".into()),
            Some(Arch::Native) | None => {}
        }

        // Defines and include folders
        flags.extend(spec.defines.iter().map(|define| format!("/D{define}")));
        flags.extend(spec.includes.iter().map(|folder| format!("/I{}", path_text(folder))));
        if !spec.system_includes.is_empty() {
            if clang_cl {
                flags.extend(spec.system_includes.iter().map(|folder| format!("/imsvc{}", path_text(folder))));
            } else {
                flags.push("/external:W0".into());
                flags.extend(spec.system_includes.iter().map(|folder| format!("/external:I{}", path_text(folder))));
            }
        }

        flags.extend(spec.flags.iter().cloned());
        flags
    }

    fn gnu_compile_flags(&self, spec: &CompileSpec) -> Vec<String> {
        let profile = spec.profile;
        let windows_clang = self.family == Family::Clang && self.is_msvc_abi();
        let mut flags: Vec<String> = vec![format!("-std={}", spec.standard)];

        // Optimization and debug info
        flags.push(
            match spec.opt {
                Opt::None => "-O0",
                Opt::Some => "-O1",
                Opt::Fast => "-O2",
                Opt::Fastest => "-O3",
                Opt::Size => "-Os",
                Opt::MinSize if self.family == Family::Clang => "-Oz",
                Opt::MinSize => "-Os",
            }
            .into(),
        );
        if profile.debug {
            flags.push("-g".into());
            if windows_clang {
                flags.push("-gcodeview".into());
            }
        }
        if !profile.assertions {
            flags.push("-DNDEBUG".into());
        }

        // clang on Windows picks the MSVC runtime itself
        if windows_clang {
            let runtime = match (profile.static_runtime, profile.debug_runtime) {
                (false, false) => "dll",
                (false, true) => "dll_dbg",
                (true, false) => "static",
                (true, true) => "static_dbg",
            };
            flags.push(format!("-fms-runtime-lib={runtime}"));
        }

        // Link-time optimization, sanitizers, position independence
        if profile.lto {
            flags.push(if self.family == Family::Clang { "-flto=thin" } else { "-flto=auto" }.into());
        }
        if !profile.sanitize.is_empty() {
            flags.push(format!("-fsanitize={}", profile.sanitize.join(",")));
            flags.push("-fno-omit-frame-pointer".into());
        }
        if spec.position_independent && !self.targets_windows() {
            flags.push("-fPIC".into());
        }

        // Warnings
        match spec.warnings {
            Warnings::Off => flags.push("-w".into()),
            Warnings::Default => flags.push("-Wall".into()),
            Warnings::High => flags.extend(["-Wall".into(), "-Wextra".into()]),
            Warnings::All => flags.extend(["-Wall".into(), "-Wextra".into(), "-Wpedantic".into()]),
        }
        if spec.werror {
            flags.push("-Werror".into());
        }

        // Instruction set: avx2 means the Haswell set, as MSVC's /arch:AVX2 does
        let haswell = ["-mavx2", "-mfma", "-mbmi", "-mbmi2", "-mf16c", "-mlzcnt", "-mpopcnt"];
        match spec.arch {
            Some(Arch::Avx) => flags.push("-mavx".into()),
            Some(Arch::Avx2) => flags.extend(haswell.iter().map(|flag| flag.to_string())),
            Some(Arch::Avx512) => {
                flags.extend(haswell.iter().map(|flag| flag.to_string()));
                flags.extend(
                    ["-mavx512f", "-mavx512bw", "-mavx512dq", "-mavx512vl"]
                        .iter()
                        .map(|flag| flag.to_string()),
                );
            }
            Some(Arch::Native) => flags.push("-march=native".into()),
            None => {}
        }

        // Defines and include folders
        flags.extend(spec.defines.iter().map(|define| format!("-D{define}")));
        flags.extend(spec.includes.iter().map(|folder| format!("-I{}", path_text(folder))));
        for folder in spec.system_includes {
            flags.extend(["-isystem".into(), path_text(folder)]);
        }

        flags.extend(spec.flags.iter().cloned());
        flags
    }

    fn msvc_link_args(&self, spec: &LinkSpec) -> Vec<String> {
        let profile = spec.profile;

        // Unoptimized debug builds relink incrementally, much faster after small changes
        let incremental = profile.debug && profile.opt == Opt::None && !profile.lto;
        let incremental_flag = if incremental { "/INCREMENTAL" } else { "/INCREMENTAL:NO" };
        let mut args: Vec<String> = vec!["/nologo".into(), incremental_flag.into(), format!("/OUT:{}", path_text(spec.output))];

        // Kind of binary
        if let Some(import_lib) = spec.import_lib {
            args.push(format!("/IMPLIB:{}", path_text(import_lib)));
        }
        if spec.shared {
            args.push("/DLL".into());
        } else if spec.gui {
            args.extend(["/SUBSYSTEM:WINDOWS".into(), "/ENTRY:mainCRTStartup".into()]);
        } else {
            args.push("/SUBSYSTEM:CONSOLE".into());
        }

        // Debug info and optimization
        if profile.debug {
            args.push("/DEBUG".into());
            if profile.opt != Opt::None {
                args.extend(["/OPT:REF".into(), "/OPT:ICF".into()]);
            }
        }
        if profile.lto && self.family == Family::Msvc {
            args.push("/LTCG".into());
        }

        // Inputs
        args.extend(spec.objects.iter().map(|object| path_text(object)));
        args.extend(spec.lib_dirs.iter().map(|folder| format!("/LIBPATH:{}", path_text(folder))));
        for lib in spec.whole {
            args.push(lib.clone());
            args.push(format!("/WHOLEARCHIVE:{lib}"));
        }
        for lib in spec.libs {
            args.push(msvc_lib_name(lib));
        }
        args.extend(WINDOWS_DEFAULT_LIBS.iter().map(|lib| format!("{lib}.lib")));

        args.extend(spec.flags.iter().cloned());
        args
    }

    fn gnu_link_args(&self, spec: &LinkSpec) -> Vec<String> {
        let profile = spec.profile;
        let windows_clang = self.family == Family::Clang && self.is_msvc_abi();
        let mut args: Vec<String> = Vec::new();

        // Kind of binary, and the import library of a DLL or exporting program on Windows
        if spec.shared {
            args.push("-shared".into());
        }
        if let Some(import_lib) = spec.import_lib {
            if windows_clang {
                args.push(format!("-Wl,/IMPLIB:{}", path_text(import_lib)));
            } else {
                args.push(format!("-Wl,--out-implib,{}", path_text(import_lib)));
            }
        }

        // ELF programs export their symbols to modules through the dynamic symbol table
        if spec.exports && self.platform.os == Os::Linux {
            args.push("-rdynamic".into());
        }
        if spec.gui && self.targets_windows() {
            args.push(
                if windows_clang {
                    "-Wl,/SUBSYSTEM:WINDOWS,/ENTRY:mainCRTStartup"
                } else {
                    "-mwindows"
                }
                .into(),
            );
        }

        // Code generation the link must match
        if profile.debug {
            args.push("-g".into());
        }
        if profile.lto {
            args.push(if self.family == Family::Clang { "-flto=thin" } else { "-flto=auto" }.into());
        }
        if !profile.sanitize.is_empty() {
            args.push(format!("-fsanitize={}", profile.sanitize.join(",")));
        }
        if profile.static_runtime && !windows_clang {
            args.push(if self.targets_windows() { "-static" } else { "-static-libstdc++" }.into());
        }

        // Shared libraries beside the executable are found at run time
        if self.platform.os == Os::Linux {
            args.push("-Wl,-rpath,$ORIGIN".into());
        }

        args.extend(spec.objects.iter().map(|object| path_text(object)));
        args.extend(spec.lib_dirs.iter().map(|folder| format!("-L{}", path_text(folder))));

        // Archives in a group so their order never matters (GNU ld only)
        let grouped = !windows_clang && self.platform.os != Os::Macos;
        if grouped {
            args.push("-Wl,--start-group".into());
        }
        if !spec.whole.is_empty() {
            if windows_clang {
                args.extend(spec.whole.iter().flat_map(|lib| [lib.clone(), format!("-Wl,/WHOLEARCHIVE:{lib}")]));
            } else if self.platform.os == Os::Macos {
                args.extend(spec.whole.iter().map(|lib| format!("-Wl,-force_load,{lib}")));
            } else {
                args.push("-Wl,--whole-archive".into());
                args.extend(spec.whole.iter().cloned());
                args.push("-Wl,--no-whole-archive".into());
            }
        }
        for lib in spec.libs {
            args.push(gnu_lib_argument(lib));
        }
        if self.targets_windows() {
            args.extend(WINDOWS_DEFAULT_LIBS.iter().map(|lib| format!("-l{lib}")));
        }
        if grouped {
            args.push("-Wl,--end-group".into());
        }

        args.push("-o".into());
        args.push(path_text(spec.output));

        args.extend(spec.flags.iter().cloned());
        args
    }
}

/// A path as a command-line argument.
pub fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn msvc_standard_flag(standard: &str, clang_cl: bool) -> Vec<String> {
    let flag = match standard {
        "c++14" => "/std:c++14",
        "c++17" | "gnu++17" => "/std:c++17",
        "c++20" | "gnu++20" => "/std:c++20",
        "c++23" | "gnu++23" if clang_cl => "/clang:-std=c++23",
        "c++23" | "gnu++23" | "c++26" | "gnu++26" => "/std:c++latest",
        "c11" => "/std:c11",
        "c17" | "c18" => "/std:c17",
        "c23" => "/std:clatest",
        _ => return Vec::new(),
    };

    vec![flag.to_string()]
}

/// "user32" -> "user32.lib"; paths and names with an extension pass through.
fn msvc_lib_name(lib: &str) -> String {
    let is_path = lib.contains('/') || lib.contains('\\');
    let has_extension = Path::new(lib).extension().is_some_and(|extension| extension == "lib" || extension == "a");

    if is_path || has_extension || lib.starts_with('/') {
        lib.to_string()
    } else {
        format!("{lib}.lib")
    }
}

/// "pthread" -> "-lpthread"; paths and flags pass through.
fn gnu_lib_argument(lib: &str) -> String {
    let is_path = lib.contains('/') || lib.contains('\\') || lib.ends_with(".a") || lib.ends_with(".so") || lib.ends_with(".lib");

    if is_path || lib.starts_with('-') {
        lib.to_string()
    } else {
        format!("-l{lib}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standards() {
        assert_eq!(normalize_standard("20", Language::Cxx).unwrap(), "c++20");
        assert_eq!(normalize_standard("c++23", Language::Cxx).unwrap(), "c++23");
        assert_eq!(normalize_standard("gnu++17", Language::Cxx).unwrap(), "gnu++17");
        assert_eq!(normalize_standard("c17", Language::C).unwrap(), "c17");
        assert!(normalize_standard("c++21", Language::Cxx).is_err());
    }

    #[test]
    fn lib_names() {
        assert_eq!(msvc_lib_name("user32"), "user32.lib");
        assert_eq!(msvc_lib_name("C:/x/fmt.lib"), "C:/x/fmt.lib");
        assert_eq!(gnu_lib_argument("pthread"), "-lpthread");
        assert_eq!(gnu_lib_argument("/usr/lib/libz.a"), "/usr/lib/libz.a");
    }
}
