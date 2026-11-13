//! Turns Anvil's compiler-neutral settings into each toolchain's command lines and file names.

use crate::platform::{Cpu, Os};
use crate::profile::{Opt, Profile};
use crate::toolchain::{Family, Toolchain};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

// The Win32 libraries every Windows program may use, linked by default as Visual Studio and CMake do; unused ones
// cost nothing
const WINDOWS_DEFAULT_LIBS: &[&str] = &[
    "kernel32", "user32", "gdi32", "winspool", "shell32", "ole32", "oleaut32", "uuid", "comdlg32", "advapi32",
];

// The folders the dynamic loader searches by itself (Linux's, with their multiarch subfolders, and macOS's): a shared
// library in them needs no rpath
const SYSTEM_LIBRARY_FOLDERS: &[&str] = &["/lib", "/lib64", "/usr/lib", "/usr/lib64", "/System/Library"];

// clang records the time stamps of the headers a precompiled header was made from and refuses it once one is touched;
// Anvil rebuilds it when their content changes, so the stamps are left out
const NO_PCH_TIMESTAMP: &[&str] = &["-Xclang", "-fno-pch-timestamp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    C,
    Cxx,
    /// Objective-C (.m) and Objective-C++ (.mm): clang and gcc only.
    ObjC,
    ObjCxx,
}

/// What kind of Windows program an executable is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gui {
    /// A console program (the default).
    Off,
    /// `gui = true`: a GUI program entered through WinMain, as CMake's WIN32 makes it.
    WinMain,
    /// `gui = "main"`: a GUI program with a standard main().
    Main,
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
    pub gui: Gui,
    /// A program that exports symbols to the modules it loads.
    pub exports: bool,
    /// A module linking against such a program: its calls into the program resolve when the program loads it.
    pub uses_program_exports: bool,
    pub objects: &'a [PathBuf],
    pub libs: &'a [String],
    /// Static libraries linked whole.
    pub whole: &'a [String],
    pub lib_dirs: &'a [PathBuf],
    pub flags: &'a [String],
    pub output: &'a Path,
    pub import_lib: Option<&'a Path>,
    /// It links C++ code (its own, or a static library's), so it takes the C++ runtime.
    pub cxx: bool,
    /// A shared library's versioned names (`version`, `soversion`).
    pub library_version: Option<&'a LibraryVersion>,
}

/// A versioned shared library's names and numbers: libfoo.so.1.2.3 (`file`, what the link writes) known as
/// libfoo.so.1 (`soname`) on Linux; libfoo.1.2.3.dylib known as libfoo.1.dylib on macOS.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryVersion {
    pub file: String,
    pub soname: String,
    pub version: String,
    pub soversion: String,
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

impl Gui {
    pub fn parse(value: &toml::Value) -> Result<Gui> {
        Ok(match value {
            toml::Value::Boolean(false) => Gui::Off,
            toml::Value::Boolean(true) => Gui::WinMain,
            toml::Value::String(text) if text == "main" => Gui::Main,
            other => bail!("gui = {other}: expected true (a GUI program with WinMain), \"main\" (a GUI program with main()) or false"),
        })
    }
}

/// "20", "c++20", "cxx20", "cpp20" -> "c++20"; "gnu++20" stays; C: "17", "c17" -> "c17"; "gnu17" stays.
pub fn normalize_standard(text: &str, language: Language) -> Result<String> {
    let lowered = text.to_ascii_lowercase();
    let cxx = matches!(language, Language::Cxx | Language::ObjCxx);
    let mut digits = lowered
        .trim_start_matches("c++")
        .trim_start_matches("cxx")
        .trim_start_matches("cpp")
        .trim_start_matches("gnu++");
    if !cxx {
        digits = digits.trim_start_matches("gnu");
    }
    let digits = digits.trim_start_matches('c');

    let valid = if cxx {
        ["11", "14", "17", "20", "23", "26"].contains(&digits)
    } else {
        ["89", "90", "99", "11", "17", "18", "23"].contains(&digits)
    };
    if !valid {
        if cxx {
            bail!("std = \"{text}\": C++ standards are 11, 14, 17, 20, 23 and 26 (\"c++20\", \"20\", or \"gnu++20\" with GNU extensions)");
        }
        bail!("c-std = \"{text}\": C standards are 89, 90, 99, 11, 17, 18 and 23 (\"c17\", \"17\", or \"gnu17\" with GNU extensions)");
    }

    Ok(if !cxx && lowered.starts_with("gnu") {
        format!("gnu{digits}")
    } else if !cxx {
        format!("c{digits}")
    } else if lowered.starts_with("gnu") {
        format!("gnu++{digits}")
    } else {
        format!("c++{digits}")
    })
}

/// A line explaining a link failure Anvil recognizes, shown under the linker's output.
pub fn link_failure_hint(output: &str) -> Option<&'static str> {
    let unresolved_winmain = output.lines().any(|line| {
        let unresolved = line.contains("LNK2019") || line.contains("LNK2001") || line.contains("undefined symbol");
        unresolved && line.contains("WinMain")
    });

    unresolved_winmain.then_some("gui = true expects WinMain; a program with main() needs gui = \"main\"")
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
            Language::C | Language::ObjC => &self.cc,
            Language::Cxx | Language::ObjCxx => &self.cxx,
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

    /// Precompiled header: the extra arguments that create it (`create`, cl-style toolchains, which make it while
    /// compiling a stub source; gcc and clang make it with `pch_header_args`) or use it.
    pub fn pch_args(&self, header: &Path, pch_file: &Path, create: bool) -> Vec<String> {
        let header = path_text(header);
        let pch_file_text = path_text(pch_file);

        match self.family {
            Family::Msvc | Family::ClangCl => {
                let mode = if create { "/Yc" } else { "/Yu" };
                let mut args = vec![format!("{mode}{header}"), format!("/Fp{pch_file_text}"), format!("/FI{header}")];
                if create && self.family == Family::ClangCl {
                    args.extend(NO_PCH_TIMESTAMP.iter().map(|flag| flag.to_string()));
                }
                args
            }
            Family::Clang => vec!["-include-pch".into(), pch_file_text],
            // gcc finds <stub>.gch beside the stub header it is told to include
            Family::Gcc => {
                let stub = pch_file.with_extension("");
                vec!["-include".into(), path_text(&stub)]
            }
        }
    }

    /// Arguments compiling a header into a precompiled header (gcc and clang).
    pub fn pch_header_args(&self, spec: &CompileSpec, header: &Path, pch_file: &Path, deps_file: &Path) -> Vec<String> {
        let mut args = self.compile_flags(spec);
        if self.family == Family::Clang {
            args.extend(NO_PCH_TIMESTAMP.iter().map(|flag| flag.to_string()));
        }

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

    /// A shared library's versioned names (CMake's VERSION and SOVERSION; a missing one is the other), from its file
    /// name: libfoo.so.1.2.3 known as libfoo.so.1 on Linux, libfoo.1.2.3.dylib as libfoo.1.dylib on macOS. None on
    /// Windows, where a DLL's name has no version, or without either number.
    pub fn library_version(&self, file_name: &str, version: Option<&str>, soversion: Option<&str>) -> Option<LibraryVersion> {
        let (version, soversion) = match (version, soversion) {
            (Some(version), Some(soversion)) => (version, soversion),
            (Some(version), None) => (version, version),
            (None, Some(soversion)) => (soversion, soversion),
            (None, None) => return None,
        };
        let named = |number: &str| match self.platform.os {
            Os::Linux => Some(format!("{file_name}.{number}")),
            Os::Macos => file_name.strip_suffix(".dylib").map(|stem| format!("{stem}.{number}.dylib")),
            Os::Windows => None,
        };

        Some(LibraryVersion {
            file: named(version)?,
            soname: named(soversion)?,
            version: version.to_string(),
            soversion: soversion.to_string(),
        })
    }

    /// The program linking a binary with no C++ code: the C compiler where the linker is the C++ one (gcc and clang
    /// outside the MSVC ABI), so it does not depend on the C++ runtime; else the linker.
    pub fn c_linker(&self) -> &Path {
        let gnu = matches!(self.family, Family::Gcc | Family::Clang) && !self.is_msvc_abi();
        if gnu && self.linker == self.cxx {
            &self.cc
        } else {
            &self.linker
        }
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

            // A 64-bit windres writes 64-bit objects unless told otherwise (32-bit builds with -m32)
            if self.platform.cpu == Cpu::X86 {
                args.push("--target=pe-i386".into());
            }
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
        let arm = matches!(self.platform.cpu, Cpu::Arm64 | Cpu::Arm);

        if self.family.is_msvc_style() && arm {
            // armasm64 takes -i and -o, and no definitions (CMake passes it none either)
            args.push("-nologo".into());
            for folder in includes {
                args.extend(["-i".into(), path_text(folder)]);
            }
            args.extend(["-o".into(), path_text(output), path_text(source)]);
        } else if self.family.is_msvc_style() {
            args.extend(["/nologo".into(), "/c".into()]);
            args.extend(includes.iter().map(|folder| format!("/I{}", path_text(folder))));
            args.extend(defines.iter().map(|define| format!("/D{define}")));
            args.push(format!("/Fo{}", path_text(output)));
            args.push(path_text(source));
        } else {
            // The compiler driver, for the machine built for (-m32, --target)
            args.extend(self.compile_target_flags.iter().cloned());
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

    /// A raw linker option as the link command takes it: as is for link.exe and lld-link, through the compiler
    /// driver (`-Wl,`) for clang and gcc.
    pub fn linker_option(&self, option: &str) -> String {
        if self.family.is_msvc_style() {
            option.to_string()
        } else {
            format!("-Wl,{option}")
        }
    }

    // Profiles

    /// Fits a profile to the toolchain: an error for what it cannot build, and the release C runtime for clang's
    /// AddressSanitizer on Windows, which has no debug one ("/MDd not allowed with -fsanitize=address").
    pub fn adapt_profile(&self, profile: &mut Profile) -> Result<()> {
        if profile.sanitize.is_empty() {
            return Ok(());
        }

        if self.family == Family::Gcc && self.targets_windows() {
            bail!(
                "profile `{}`: sanitize needs a sanitizer runtime, which MinGW gcc does not have; build it with --toolchain msvc, clang-cl or clang",
                profile.name
            );
        }
        if self.is_msvc_abi() && self.family != Family::Msvc {
            // clang's AddressSanitizer runtime, where the compiler said it is when it was detected
            let address = profile.sanitize.iter().any(|sanitizer| sanitizer == "address");
            if address && self.sanitizer_runtime_dir.is_none() {
                bail!(
                    "profile `{}`: sanitize = [\"address\"] needs clang's AddressSanitizer runtime (clang_rt.asan_dynamic-{}.lib), which {} does not find; \
                     install LLVM's compiler-rt libraries (the LLVM installer and Visual Studio's \"C++ Clang tools\" have them) and run \
                     `anvil toolchains --refresh`, or build it with --toolchain msvc",
                    profile.name,
                    self.asan_architecture(),
                    crate::util::slash(&self.cxx)
                );
            }

            if profile.debug_runtime_requested {
                crate::ui::warn(&format!(
                    "profile `{}`: debug-runtime = true is not used with sanitize on {}: clang's sanitizers on Windows have no debug C runtime, so it links the release one",
                    profile.name,
                    self.family.name()
                ));
            }
            profile.debug_runtime = false;
        }

        Ok(())
    }

    /// The AddressSanitizer runtime DLLs a sanitized Windows program needs beside it, when the profile asks for
    /// them: cl's lie beside cl (the debug one too: which one a program uses depends on the MSVC version), clang's in
    /// its resource folder.
    pub fn sanitizer_runtime(&self, profile: &Profile) -> Result<Vec<PathBuf>> {
        if !self.is_msvc_abi() || !profile.sanitize.iter().any(|sanitizer| sanitizer == "address") {
            return Ok(Vec::new());
        }

        let architecture = self.asan_architecture();
        let (folders, names): (Vec<PathBuf>, Vec<String>) = match self.family {
            Family::Msvc => (
                self.cxx.parent().into_iter().map(Path::to_path_buf).collect(),
                vec![
                    format!("clang_rt.asan_dynamic-{architecture}.dll"),
                    format!("clang_rt.asan_dbg_dynamic-{architecture}.dll"),
                ],
            ),
            _ => (
                self.sanitizer_runtime_dir.iter().cloned().collect(),
                vec![format!("clang_rt.asan_dynamic-{architecture}.dll")],
            ),
        };

        let Some(folder) = folders.iter().find(|folder| folder.join(&names[0]).is_file()) else {
            let searched: Vec<String> = folders.iter().map(|folder| crate::util::slash(folder)).collect();
            bail!("sanitize: the AddressSanitizer runtime {} is not in {}", names[0], searched.join(", "));
        };

        Ok(names.iter().map(|name| folder.join(name)).filter(|file| file.is_file()).collect())
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

        // Objective-C sources say so, with the C or C++ standard
        match spec.language {
            Language::ObjC => flags.extend(["-x".into(), "objective-c".into()]),
            Language::ObjCxx => flags.extend(["-x".into(), "objective-c++".into()]),
            Language::C | Language::Cxx => {}
        }

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
            flags.push(clang_ms_runtime_flag(profile));
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

        // Unoptimized debug builds relink incrementally, much faster after small changes (not with ASan: LNK4300)
        let incremental = profile.debug && profile.opt == Opt::None && !profile.lto && profile.sanitize.is_empty();
        let incremental_flag = if incremental { "/INCREMENTAL" } else { "/INCREMENTAL:NO" };
        let mut args: Vec<String> = vec!["/nologo".into(), incremental_flag.into(), format!("/OUT:{}", path_text(spec.output))];

        // Kind of binary
        if let Some(import_lib) = spec.import_lib {
            args.push(format!("/IMPLIB:{}", path_text(import_lib)));
        }
        if spec.shared {
            args.push("/DLL".into());
        } else {
            args.extend(msvc_subsystem(spec.gui).iter().map(|option| option.to_string()));
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

        // lld-link gets the AddressSanitizer runtime the clang-cl driver would add (link.exe infers cl's itself)
        if self.family == Family::ClangCl {
            args.extend(self.clang_asan_libs(profile));
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
        let linux = self.platform.os == Os::Linux;
        let macos = self.platform.os == Os::Macos;
        let file_name = spec.output.file_name().unwrap_or_default().to_string_lossy().into_owned();
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

        // A shared library is known by its file name (its ABI version's, when versioned), not its build path, so its
        // users find it beside them (rpath)
        let soname = spec.library_version.map_or(file_name.as_str(), |version| version.soname.as_str());
        if spec.shared && linux {
            args.push(format!("-Wl,-soname,{soname}"));
        }
        if spec.shared && macos {
            args.push(format!("-Wl,-install_name,@rpath/{soname}"));
        }
        if let Some(version) = spec.library_version.filter(|_| spec.shared && macos) {
            args.push(format!("-Wl,-current_version,{}", version.version));
            args.push(format!("-Wl,-compatibility_version,{}", version.soversion));
        }

        // ELF programs export their symbols to modules through the dynamic symbol table; a Mach-O module's calls
        // into the program that loads it are resolved then (ELF allows that by default)
        if spec.exports && linux {
            args.push("-rdynamic".into());
        }
        if spec.uses_program_exports && macos {
            args.push("-Wl,-undefined,dynamic_lookup".into());
        }
        if self.targets_windows() && !spec.shared {
            args.extend(self.gnu_subsystem(spec.gui));
        }

        // Code generation the link must match (the MSVC runtime decides which sanitizer thunk clang links); clang's
        // LTO links through lld when it is installed
        if profile.debug {
            args.push("-g".into());
        }
        if windows_clang {
            args.push(clang_ms_runtime_flag(profile));
        }
        if profile.lto {
            args.push(if self.family == Family::Clang { "-flto=thin" } else { "-flto=auto" }.into());
            if self.family == Family::Clang && self.lld {
                args.push("-fuse-ld=lld".into());
            }
        }
        // Sanitizers: on Windows clang's driver links the static C runtime's AddressSanitizer thunk whatever
        // -fms-runtime-lib says, so it gets the libraries clang-cl would link instead
        let driver_sanitizers: Vec<&str> = profile
            .sanitize
            .iter()
            .map(String::as_str)
            .filter(|sanitizer| !(windows_clang && *sanitizer == "address"))
            .collect();
        if !driver_sanitizers.is_empty() {
            args.push(format!("-fsanitize={}", driver_sanitizers.join(",")));
        }
        if windows_clang && !profile.sanitize.is_empty() {
            let asan_libs = self.clang_asan_libs(profile);
            args.extend(
                asan_libs
                    .iter()
                    .map(|arg| if arg.starts_with('/') { format!("-Wl,{arg}") } else { arg.clone() }),
            );
            args.push("-Wl,/INCREMENTAL:NO".into());
        }
        // The static C++ runtime (not Apple clang's libc++, which has none to link)
        if profile.static_runtime && !windows_clang && self.targets_windows() {
            args.push("-static".into());
        } else if profile.static_runtime && !windows_clang && spec.cxx && !macos {
            args.push("-static-libstdc++".into());
        }

        // Shared libraries beside the binary are found at run time, and from bin/ in lib/ as `anvil install` lays
        // them out, so installing needs no relink
        if linux {
            args.push("-Wl,-rpath,$ORIGIN".into());
            args.push("-Wl,-rpath,$ORIGIN/../lib".into());
        }
        if macos {
            args.push("-Wl,-rpath,@loader_path".into());
            args.push("-Wl,-rpath,@loader_path/../lib".into());
        }

        // Shared libraries linked from folders of their own (pkg-config's, prebuilt ones, a dynamic vcpkg triplet's)
        // are found where they are, as in CMake's build tree
        if linux || macos {
            for folder in shared_library_folders(spec) {
                args.push(format!("-Wl,-rpath,{}", path_text(&folder)));
            }
        }

        // The target's and profile's options go before the inputs, as CMake puts them (-Wl,--as-needed); the libraries
        // among them after the objects, since GNU ld only resolves a library against what came before it
        let (flag_options, flag_libraries) = split_link_flags(spec.flags);
        args.extend(flag_options);

        args.extend(spec.objects.iter().map(|object| path_text(object)));
        args.extend(spec.lib_dirs.iter().map(|folder| format!("-L{}", path_text(folder))));

        // Archives in a group so their order never matters (GNU ld only)
        let grouped = !windows_clang && !macos;
        if grouped {
            args.push("-Wl,--start-group".into());
        }
        if !spec.whole.is_empty() {
            if windows_clang {
                args.extend(spec.whole.iter().flat_map(|lib| [lib.clone(), format!("-Wl,/WHOLEARCHIVE:{lib}")]));
            } else if macos {
                args.extend(spec.whole.iter().map(|lib| format!("-Wl,-force_load,{lib}")));
            } else {
                args.push("-Wl,--whole-archive".into());
                args.extend(spec.whole.iter().cloned());
                args.push("-Wl,--no-whole-archive".into());
            }
        }
        args.extend(flag_libraries);

        // A flag's value passes as it is ("-framework", "Cocoa")
        let mut flag_value = false;
        for lib in spec.libs {
            if flag_value {
                args.push(lib.clone());
            } else {
                args.extend(gnu_lib_arguments(lib));
            }
            flag_value = takes_value(lib);
        }
        if self.targets_windows() {
            args.extend(WINDOWS_DEFAULT_LIBS.iter().map(|lib| format!("-l{lib}")));
        }
        if grouped {
            args.push("-Wl,--end-group".into());
        }

        args.push("-o".into());
        args.push(path_text(spec.output));
        args
    }

    /// A Windows program's subsystem and entry through the compiler driver: MSVC-ABI clang hands link.exe the
    /// options; MinGW's -mwindows suits WinMain and main() alike (its runtime calls whichever the program has).
    fn gnu_subsystem(&self, gui: Gui) -> Vec<String> {
        if self.family == Family::Clang && self.is_msvc_abi() {
            let options = msvc_subsystem(gui).join(",");
            vec![format!("-Wl,{options}")]
        } else if gui != Gui::Off {
            vec!["-mwindows".into()]
        } else {
            Vec::new()
        }
    }

    /// The AddressSanitizer libraries `clang-cl -fsanitize=address` links (lld-link and clang's driver get them from
    /// Anvil): the runtime's import library, and the thunk for the program's C runtime linked whole.
    fn clang_asan_libs(&self, profile: &Profile) -> Vec<String> {
        let architecture = self.asan_architecture();
        let runtime = format!("clang_rt.asan_dynamic-{architecture}.lib");
        let address = profile.sanitize.iter().any(|sanitizer| sanitizer == "address");
        let Some(folder) = self.sanitizer_runtime_dir.as_ref().filter(|_| address) else {
            return Vec::new();
        };

        let mut args = vec![path_text(&folder.join(runtime))];
        if profile.static_runtime {
            args.push(format!(
                "/WHOLEARCHIVE:{}",
                path_text(&folder.join(format!("clang_rt.asan_static_runtime_thunk-{architecture}.lib")))
            ));
        } else {
            let underscore = if self.platform.cpu == Cpu::X86 { "_" } else { "" };
            args.push(format!("/INCLUDE:{underscore}__asan_seh_interceptor"));
            args.push(format!(
                "/WHOLEARCHIVE:{}",
                path_text(&folder.join(format!("clang_rt.asan_dynamic_runtime_thunk-{architecture}.lib")))
            ));
        }

        args
    }

    /// The processor in the sanitizer runtime's file names.
    fn asan_architecture(&self) -> &'static str {
        asan_architecture(self.platform.cpu)
    }
}

/// The processor in the sanitizer runtime's file names (clang_rt.asan_dynamic-x86_64.lib).
pub fn asan_architecture(cpu: Cpu) -> &'static str {
    match cpu {
        Cpu::X64 => "x86_64",
        Cpu::X86 => "i386",
        Cpu::Arm64 => "aarch64",
        Cpu::Arm => "armhf",
    }
}

/// A path as a command-line argument.
pub fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A linker flag in `libs` whose value is the next entry (`"-framework", "Cocoa"`).
pub fn takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "-framework" | "-weak_framework" | "-needed_framework" | "-Xlinker" | "-u" | "-T" | "-L" | "-F" | "-l" | "-z"
    )
}

// GNU ld switches that change how the libraries after them link: a run of them with libraries keeps its place
const POSITIONAL_LINKER_SWITCHES: &[&str] = &[
    "--whole-archive",
    "--no-whole-archive",
    "-Bstatic",
    "-Bdynamic",
    "-static",
    "--start-group",
    "--end-group",
    "-(",
    "-)",
    "--as-needed",
    "--no-as-needed",
    "--push-state",
    "--pop-state",
];

/// What one link-flags entry (with its value) is to the order of a GNU link.
#[derive(Debug, PartialEq)]
enum LinkFlagKind {
    /// A library or object file, or `-lname`.
    Library,
    /// A switch from POSITIONAL_LINKER_SWITCHES.
    Positional,
    Option,
}

/// Splits link flags into the options that go before the objects and the libraries that go after them: library
/// files, `-l` names, and runs of positional switches around them (-Wl,--whole-archive x.a -Wl,--no-whole-archive)
/// as a unit, in their order.
fn split_link_flags(flags: &[String]) -> (Vec<String>, Vec<String>) {
    let mut options = Vec::new();
    let mut libraries = Vec::new();
    let mut run: Vec<String> = Vec::new();
    let mut run_has_library = false;

    let mut index = 0;
    while index < flags.len() {
        // An option and its value stay together ("-Xlinker", "--whole-archive")
        let length = if takes_value(&flags[index]) && index + 1 < flags.len() { 2 } else { 1 };
        let piece = &flags[index..index + length];
        index += length;

        match link_flag_kind(piece) {
            LinkFlagKind::Library => {
                run.extend(piece.iter().cloned());
                run_has_library = true;
            }
            LinkFlagKind::Positional => run.extend(piece.iter().cloned()),
            LinkFlagKind::Option => {
                if run_has_library { &mut libraries } else { &mut options }.append(&mut run);
                run_has_library = false;
                options.extend(piece.iter().cloned());
            }
        }
    }
    if run_has_library { &mut libraries } else { &mut options }.append(&mut run);

    (options, libraries)
}

fn link_flag_kind(piece: &[String]) -> LinkFlagKind {
    let first = piece[0].as_str();
    if first == "-l" || (first.starts_with("-l") && first.len() > 2) || is_library_file(first) {
        return LinkFlagKind::Library;
    }

    // What the linker itself gets: -Wl,a,b or -Xlinker a
    let linker_words: Vec<&str> = match first.strip_prefix("-Wl,") {
        Some(list) => list.split(',').collect(),
        None if first == "-Xlinker" => piece[1..].iter().map(String::as_str).collect(),
        None => Vec::new(),
    };
    if linker_words
        .iter()
        .any(|word| is_library_file(word) || (word.starts_with("-l") && word.len() > 2))
    {
        LinkFlagKind::Library
    } else if !linker_words.is_empty() && linker_words.iter().all(|word| POSITIONAL_LINKER_SWITCHES.contains(word)) {
        LinkFlagKind::Positional
    } else {
        LinkFlagKind::Option
    }
}

/// A library or object file a link takes as an input (libz.a, libz.so.1, z.lib, crt.o).
fn is_library_file(word: &str) -> bool {
    let lowered = word.to_ascii_lowercase();
    let extensions = [".a", ".so", ".lib", ".dylib", ".tbd", ".o", ".obj"];
    !word.starts_with('-') && (extensions.iter().any(|extension| lowered.ends_with(extension)) || lowered.contains(".so."))
}

fn msvc_standard_flag(standard: &str, clang_cl: bool) -> Vec<String> {
    let flag = match standard {
        "c++14" => "/std:c++14",
        "c++17" | "gnu++17" => "/std:c++17",
        "c++20" | "gnu++20" => "/std:c++20",
        "c++23" | "gnu++23" if clang_cl => "/clang:-std=c++23",
        "c++23" | "gnu++23" | "c++26" | "gnu++26" => "/std:c++latest",
        "c11" | "gnu11" => "/std:c11",
        "c17" | "c18" | "gnu17" | "gnu18" => "/std:c17",
        "c23" | "gnu23" => "/std:clatest",
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

/// clang's choice of the MSVC runtime library (-fms-runtime-lib), for compiling and linking.
fn clang_ms_runtime_flag(profile: &Profile) -> String {
    let runtime = match (profile.static_runtime, profile.debug_runtime) {
        (false, false) => "dll",
        (false, true) => "dll_dbg",
        (true, false) => "static",
        (true, true) => "static_dbg",
    };
    format!("-fms-runtime-lib={runtime}")
}

/// link.exe's subsystem and entry options for a program.
fn msvc_subsystem(gui: Gui) -> &'static [&'static str] {
    match gui {
        Gui::Off => &["/SUBSYSTEM:CONSOLE"],
        Gui::WinMain => &["/SUBSYSTEM:WINDOWS"],
        Gui::Main => &["/SUBSYSTEM:WINDOWS", "/ENTRY:mainCRTStartup"],
    }
}

/// The folders of the shared libraries a link names (in `libs` or among its flags), by path or by name in one of its
/// library folders, other than the output's folder and the system's; each once.
fn shared_library_folders(spec: &LinkSpec) -> Vec<PathBuf> {
    let is_shared = |name: &str| name.ends_with(".so") || name.contains(".so.") || name.ends_with(".dylib");
    let output_folder = spec.output.parent().map(crate::util::normalize);
    let mut folders: Vec<PathBuf> = Vec::new();
    let mut add = |folder: &Path| {
        let folder = crate::util::normalize(folder);
        let system = SYSTEM_LIBRARY_FOLDERS.iter().any(|system| folder.starts_with(system));
        if !system && Some(&folder) != output_folder.as_ref() && !folders.contains(&folder) {
            folders.push(folder);
        }
    };

    // The library folders: the link's, and -L ones among its flags
    let mut library_folders: Vec<PathBuf> = spec.lib_dirs.to_vec();
    library_folders.extend(
        spec.flags
            .iter()
            .filter_map(|flag| flag.strip_prefix("-L"))
            .filter(|folder| !folder.is_empty())
            .map(PathBuf::from),
    );

    let mut previous: Option<&String> = None;
    for word in spec.libs.iter().chain(spec.flags) {
        // A flag's value ("-framework", "Cocoa"), or another flag
        let is_value = previous.is_some_and(|flag| takes_value(flag));
        previous = Some(word);
        let name = match word.strip_prefix("-l") {
            Some(name) => name,
            None if is_value || word.starts_with('-') => continue,
            None => word.as_str(),
        };

        // A file, or a name found in one of the library folders
        let path = Path::new(name);
        if path.is_absolute() && is_shared(name) {
            add(path.parent().unwrap_or(path));
        } else if !name.contains(['/', '\\', '.']) {
            let holds = |folder: &&PathBuf| {
                ["so", "dylib"]
                    .iter()
                    .any(|extension| folder.join(format!("lib{name}.{extension}")).is_file())
            };
            if let Some(folder) = library_folders.iter().find(holds) {
                add(folder);
            }
        }
    }

    folders
}

/// "pthread" -> "-lpthread"; "Cocoa.framework" -> "-framework Cocoa" (a path to one also gets its folder searched);
/// paths and flags pass through.
fn gnu_lib_arguments(lib: &str) -> Vec<String> {
    let is_path = lib.contains('/') || lib.contains('\\');

    if let Some(framework_path) = lib.strip_suffix(".framework") {
        let framework = Path::new(framework_path);
        let name = framework.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let mut arguments = Vec::new();
        if is_path {
            arguments.push(format!("-F{}", path_text(framework.parent().unwrap_or(Path::new(".")))));
        }
        arguments.extend(["-framework".to_string(), name]);
        return arguments;
    }

    let is_file = is_path || lib.ends_with(".a") || lib.ends_with(".so") || lib.ends_with(".lib") || lib.ends_with(".dylib");
    if is_file || lib.starts_with('-') {
        vec![lib.to_string()]
    } else {
        vec![format!("-l{lib}")]
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

        // A wrong one names what is accepted
        let error = normalize_standard("c++99", Language::Cxx).unwrap_err().to_string();
        assert!(error.contains("std = \"c++99\": C++ standards are 11, 14, 17, 20, 23 and 26"), "{error}");
        let error = normalize_standard("c98", Language::C).unwrap_err().to_string();
        assert!(error.contains("c-std = \"c98\": C standards are 89, 90, 99"), "{error}");
    }

    #[test]
    fn lib_names() {
        assert_eq!(msvc_lib_name("user32"), "user32.lib");
        assert_eq!(msvc_lib_name("C:/x/fmt.lib"), "C:/x/fmt.lib");
        assert_eq!(gnu_lib_arguments("pthread"), ["-lpthread"]);
        assert_eq!(gnu_lib_arguments("/usr/lib/libz.a"), ["/usr/lib/libz.a"]);
        assert_eq!(gnu_lib_arguments("Cocoa.framework"), ["-framework", "Cocoa"]);
        assert_eq!(gnu_lib_arguments("/opt/sdk/Foo.framework"), ["-F/opt/sdk", "-framework", "Foo"]);
    }

    fn platform(cpu: Cpu, os: Os) -> crate::platform::Platform {
        crate::platform::Platform { cpu, os, triple: None }
    }

    fn link(toolchain: &Toolchain, shared: bool, gui: Gui, uses_program_exports: bool) -> Vec<String> {
        link_libs(toolchain, shared, gui, uses_program_exports, &["m".to_string()])
    }

    fn link_libs(toolchain: &Toolchain, shared: bool, gui: Gui, uses_program_exports: bool, libs: &[String]) -> Vec<String> {
        let profile = Profile::resolve("debug", &Default::default()).unwrap();
        let spec = LinkSpec {
            profile: &profile,
            shared,
            gui,
            exports: false,
            uses_program_exports,
            objects: &[PathBuf::from("main.o")],
            libs,
            whole: &[],
            lib_dirs: &[],
            flags: &["-Wl,--as-needed".to_string()],
            output: Path::new("/out/libplugin.so"),
            import_lib: None,
            cxx: true,
            library_version: None,
        };
        toolchain.link_args(&spec)
    }

    #[test]
    fn gui_programs() {
        assert_eq!(Gui::parse(&toml::Value::Boolean(true)).unwrap(), Gui::WinMain);
        assert_eq!(Gui::parse(&toml::Value::String("main".into())).unwrap(), Gui::Main);
        assert!(Gui::parse(&toml::Value::String("winmain".into())).is_err());

        let msvc = Toolchain::for_tests(Family::Msvc, platform(Cpu::X64, Os::Windows), true);
        let winmain = link(&msvc, false, Gui::WinMain, false);
        assert!(winmain.contains(&"/SUBSYSTEM:WINDOWS".to_string()) && !winmain.iter().any(|arg| arg.starts_with("/ENTRY")));
        assert!(link(&msvc, false, Gui::Main, false).contains(&"/ENTRY:mainCRTStartup".to_string()));

        let clang = Toolchain::for_tests(Family::Clang, platform(Cpu::X64, Os::Windows), true);
        assert!(link(&clang, false, Gui::Main, false).contains(&"-Wl,/SUBSYSTEM:WINDOWS,/ENTRY:mainCRTStartup".to_string()));
        let mingw = Toolchain::for_tests(Family::Gcc, platform(Cpu::X64, Os::Windows), false);
        assert!(link(&mingw, false, Gui::Main, false).contains(&"-mwindows".to_string()));

        let error = "app.obj : error LNK2019: unresolved external symbol WinMain referenced in function \"int __cdecl invoke_main(void)\"";
        assert!(link_failure_hint(error).is_some());
        assert!(link_failure_hint("lld-link: error: undefined symbol: WinMain\n>>> msvcrtd.lib(exe_winmain.obj)").is_some());
        assert!(link_failure_hint("error LNK2019: unresolved external symbol foo").is_none());
    }

    #[test]
    fn shared_libraries_name_themselves() {
        let linux = Toolchain::for_tests(Family::Gcc, platform(Cpu::X64, Os::Linux), false);
        let args = link(&linux, true, Gui::Off, false);
        assert!(args.contains(&"-Wl,-soname,libplugin.so".to_string()));
        assert!(args.contains(&"-Wl,-rpath,$ORIGIN".to_string()));
        assert!(args.contains(&"-Wl,-rpath,$ORIGIN/../lib".to_string()));

        // The target's flags come before the objects and libraries they affect
        let flag = args.iter().position(|arg| arg == "-Wl,--as-needed").unwrap();
        assert!(flag < args.iter().position(|arg| arg == "main.o").unwrap());
        assert!(flag < args.iter().position(|arg| arg == "-lm").unwrap());

        let macos = Toolchain::for_tests(Family::Clang, platform(Cpu::Arm64, Os::Macos), false);
        let args = link(&macos, true, Gui::Off, true);
        assert!(args.contains(&"-Wl,-install_name,@rpath/libplugin.so".to_string()));
        assert!(args.contains(&"-Wl,-rpath,@loader_path".to_string()));
        assert!(args.contains(&"-Wl,-rpath,@loader_path/../lib".to_string()));
        assert!(args.contains(&"-Wl,-undefined,dynamic_lookup".to_string()));

        // Frameworks, as a name or a flag and its value
        let libs = ["Cocoa.framework", "-framework", "Metal"].map(String::from);
        let args = link_libs(&macos, false, Gui::Off, false, &libs);
        assert!(args.windows(2).any(|pair| pair == ["-framework", "Cocoa"]));
        assert!(args.windows(2).any(|pair| pair == ["-framework", "Metal"]));
    }

    #[test]
    fn shared_libraries_from_other_folders_are_found_where_they_are() {
        // A prebuilt library by path, a name in a library folder, the program's own folder, the system's, one in the
        // link flags (as pkg-config gives it)
        let root = std::env::temp_dir().join(format!("anvil_rpath_{}", std::process::id()));
        let named = root.join("named").join("lib");
        std::fs::create_dir_all(&named).unwrap();
        std::fs::write(named.join("libbar.so"), "").unwrap();
        let output = root.join("out/app");
        let libs = [
            path_text(&root.join("sdk/lib/../lib/libfoo.so.1")),
            "bar".to_string(),
            path_text(&root.join("out/libown.so")),
            "/usr/lib/x86_64-linux-gnu/libz.so".to_string(),
            "-framework".to_string(),
            "Cocoa".to_string(),
        ];
        let flags = ["-Wl,--as-needed".to_string(), path_text(&root.join("flagged").join("libbaz.so"))];
        let profile = Profile::resolve("debug", &Default::default()).unwrap();
        let spec = LinkSpec {
            profile: &profile,
            shared: false,
            gui: Gui::Off,
            exports: false,
            uses_program_exports: false,
            objects: &[PathBuf::from("main.o")],
            libs: &libs,
            whole: &[],
            lib_dirs: &[root.join("empty"), named.clone()],
            flags: &flags,
            output: &output,
            import_lib: None,
            cxx: true,
            library_version: None,
        };

        let linux = Toolchain::for_tests(Family::Gcc, platform(Cpu::X64, Os::Linux), false);
        let args = linux.link_args(&spec);
        let rpaths: Vec<&String> = args.iter().filter(|arg| arg.starts_with("-Wl,-rpath,")).collect();
        let expected = [
            "-Wl,-rpath,$ORIGIN".to_string(),
            "-Wl,-rpath,$ORIGIN/../lib".to_string(),
            format!("-Wl,-rpath,{}", path_text(&root.join("sdk").join("lib"))),
            format!("-Wl,-rpath,{}", path_text(&named)),
            format!("-Wl,-rpath,{}", path_text(&root.join("flagged"))),
        ];
        assert_eq!(rpaths, expected.iter().collect::<Vec<_>>());

        // Windows programs find DLLs on PATH or beside them
        let mingw = Toolchain::for_tests(Family::Gcc, platform(Cpu::X64, Os::Windows), false);
        assert!(!mingw.link_args(&spec).iter().any(|arg| arg.starts_with("-Wl,-rpath,")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn libraries_in_link_flags_follow_the_objects() {
        let flags = [
            "-Wl,--as-needed",
            "/out/libhelper.a",
            "-lws2_32",
            "-Wl,--whole-archive",
            "x.a",
            "-Wl,--no-whole-archive",
            "-Xlinker",
            "--gc-sections",
            "-Wl,-Bstatic",
            "-l",
            "z",
        ]
        .map(String::from);
        let (options, libraries) = split_link_flags(&flags);
        assert_eq!(options, ["-Xlinker", "--gc-sections"]);
        assert_eq!(
            libraries,
            [
                "-Wl,--as-needed",
                "/out/libhelper.a",
                "-lws2_32",
                "-Wl,--whole-archive",
                "x.a",
                "-Wl,--no-whole-archive",
                "-Wl,-Bstatic",
                "-l",
                "z"
            ]
        );

        // An option alone stays before the objects; a library goes after them
        let (options, libraries) = split_link_flags(&["-Wl,-Bstatic".to_string(), "-static".to_string()]);
        assert_eq!((options.len(), libraries.len()), (2, 0));

        let linux = Toolchain::for_tests(Family::Gcc, platform(Cpu::X64, Os::Linux), false);
        let profile = Profile::resolve("debug", &Default::default()).unwrap();
        let spec = LinkSpec {
            profile: &profile,
            shared: false,
            gui: Gui::Off,
            exports: false,
            uses_program_exports: false,
            objects: &[PathBuf::from("main.o")],
            libs: &[],
            whole: &[],
            lib_dirs: &[],
            flags: &["/out/libhelper.a".to_string()],
            output: Path::new("/out/app"),
            import_lib: None,
            cxx: true,
            library_version: None,
        };
        let args = linux.link_args(&spec);
        let position = |wanted: &str| args.iter().position(|arg| arg == wanted).unwrap();
        assert!(position("main.o") < position("/out/libhelper.a"));
    }

    #[test]
    fn precompiled_headers_ignore_time_stamps() {
        let clang_cl = Toolchain::for_tests(Family::ClangCl, platform(Cpu::X64, Os::Windows), true);
        let create = clang_cl.pch_args(Path::new("pch.h"), Path::new("pch.pch"), true);
        assert!(create.windows(2).any(|pair| pair == ["-Xclang", "-fno-pch-timestamp"]));
        assert!(!clang_cl
            .pch_args(Path::new("pch.h"), Path::new("pch.pch"), false)
            .contains(&"-Xclang".to_string()));
    }

    #[test]
    fn assembly() {
        let mut gcc = Toolchain::for_tests(Family::Gcc, platform(Cpu::X86, Os::Windows), false);
        gcc.compile_target_flags = vec!["-m32".into()];
        assert_eq!(gcc.assemble_args(Path::new("a.s"), Path::new("a.o"), &[], &[])[0], "-m32");

        let arm = Toolchain::for_tests(Family::Msvc, platform(Cpu::Arm64, Os::Windows), true);
        let args = arm.assemble_args(Path::new("a.asm"), Path::new("a.obj"), &[PathBuf::from("inc")], &["X".into()]);
        assert_eq!(args, ["-nologo", "-i", "inc", "-o", "a.obj", "a.asm"]);
    }

    #[test]
    fn mingw_has_no_sanitizers() {
        let mut profile = Profile::resolve("debug", &Default::default()).unwrap();
        profile.sanitize = vec!["address".into()];

        let mingw = Toolchain::for_tests(Family::Gcc, platform(Cpu::X64, Os::Windows), false);
        assert!(mingw.adapt_profile(&mut profile.clone()).is_err());

        let mut clang_cl = Toolchain::for_tests(Family::ClangCl, platform(Cpu::X64, Os::Windows), true);
        assert!(clang_cl
            .adapt_profile(&mut profile.clone())
            .unwrap_err()
            .to_string()
            .contains("AddressSanitizer runtime"));
        clang_cl.sanitizer_runtime_dir = Some(PathBuf::from("/llvm/lib/clang/22/lib/windows"));
        clang_cl.adapt_profile(&mut profile).unwrap();
        assert!(!profile.debug_runtime);
    }
}
