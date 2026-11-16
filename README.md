# Anvil

A fast, zero-ceremony build system for C and C++, written in Rust.

- **No configure step.** `anvil build` reads `anvil.toml`, plans, and builds. There is no build folder to generate, no cache to delete.
- **Small manifests.** Conventions cover the common layout, so a project often needs only a `[project]` table.
- **vcpkg built in.** Anvil finds every vcpkg on the machine, installs what `[deps]` asks for, and wires up includes, libraries and DLLs.
- **Not error prone.** Every table rejects unknown keys (`linkflags` gets "did you mean `link-flags`?"), every condition, preset, profile and dependency is checked whether this build uses it or not, a target's settings are its own unless it says `public`, and rebuilds are decided by content hashes and full command lines.
- **CMake conversion.** `anvil convert` runs a CMake project's `CMakeLists.txt` through an interpreter and writes an equivalent `anvil.toml`.

## Quick start

```sh
anvil new hello          # program: src/main.cpp, anvil.toml, .gitignore
cd hello
anvil run                # build and run
anvil build --release
anvil add fmt --to hello # vcpkg port, added to vcpkg.json and [deps]
```

`anvil new mylib --lib` creates a library with a public header in `include/mylib/` and a test in `tests/`.

## Breaking changes in this version

- Generated files go to `out/<profile>/generated/<target>/` instead of `gen/` (off Windows, `gen` may be a program beside them). `{gen}` follows; paths written out by hand do not.
- `c-std` defaults to `gnu17`, as gcc and CMake compile C (`/std:c17` with MSVC). GNU C predefines `unix` and `linux` on Linux, which breaks code using them as names: write `c-std = "17"` for strict ISO C.
- Objects link in the order the sources are listed, whatever their language (a glob's files sorted by path), as CMake links them; a target whose order changes relinks once.
- `anvil run` of a test target runs it as its test does, with its `args`, `workdir` and `env`.
- With gcc and clang outside the MSVC ABI, a binary with no C++ in it or in the static libraries it links links through the C compiler, without the C++ runtime; `link-language = "c++"` links it as C++.
- A panic in Anvil (a bug) unwinds in release builds too: Anvil exits with 101 instead of aborting.

## Commands

| Command | What it does |
|---|---|
| `anvil build [targets]` (`b`) | Build everything but tests. `--all` adds tests. |
| `anvil run [target] -- args` (`r`) | Build and run a program. A test target runs as its test does (its `args`, `workdir` and `env`, then the arguments after `--`). |
| `anvil test [filter] -- args` (`t`) | Build and run tests; arguments after `--` go to the tests whose command starts with `{target:...}`. `--list` names the tests without building (none is not a failure); `--rerun-failed` runs those that failed last time; `--show-output` prints passing tests' output too; `--junit <file>` writes a JUnit report. |
| `anvil install [targets]` | Build (release, unless `-p` names another profile), then copy programs, libraries and headers into a prefix (`--prefix <dir>`, default `out/install`). See Installing. |
| `anvil clean [<targets>] [-r \| -p <name>] [--toolchain <name>] [--target <machine>] [--all]` | Delete a profile's output folders (its toolchain and machine variants, `debug-clang`, `debug-arm64`, too; never another profile's; with `--toolchain` or `--target`, only that one's folder). Named targets delete only their objects and their program or library (with its import library), waiting for a build of the folder to finish: what an IDE's Clean and Rebuild of one project do (`-o` sets the options that find them). `--all` deletes the whole output folder, vcpkg packages and a `--junit` report or `--prefix` inside it included, only when it holds nothing but what Anvil wrote. |
| `anvil new <name> [--lib]` | Create a project folder. |
| `anvil init` | Create `anvil.toml` (and `.gitignore`) for the sources here: the conventions when they fit, else a program per folder with a main file and a library of the rest. Converts the `CMakeLists.txt` here instead when there is one. |
| `anvil add <name>` | Add a dependency to the `anvil.toml` you are in: vcpkg by default; `--git` (with `--tag`, `--branch` or `--rev`), `--path`, `--pkg-config`, `--system`; `--features`, `--to <target>`, `--dev` for `[dev-deps]`. |
| `anvil remove <name>` | Remove a `[deps]` or `[dev-deps]` entry from the `anvil.toml` you are in, with every use of it (lists and `when` blocks left empty go too), and its port from `vcpkg.json` when nothing else needs it. |
| `anvil convert [path]` | Convert a CMake project (`--stdout` to preview, `--force` to overwrite). |
| `anvil convert --check` | Configure the CMake project too and compare both builds' compile commands, file by file. Only with `--check` does `convert` take the planning options. |
| `anvil targets` | List targets (fetches, installs and writes nothing). `--json` prints the targets, tests and toolchain for editors (see IDEs). |
| `anvil ide [vs\|vscode\|clion\|zed\|all]` | Write an IDE's project files, which every build then keeps up to date. See IDEs. |
| `anvil lsp` | Run the language server for `anvil.toml` and `tests.toml`. See Language server. |
| `anvil toolchains` (`doctor`) | Show the compilers Anvil can use. `--refresh` detects them again instead of using the cache; `--set-default <name>` picks the one for projects that name none. |
| `anvil vcpkg ...` | List, choose or set up vcpkg installations, or run vcpkg itself. |
| `anvil compdb` | Write `compile_commands.json` to the project root for clangd; every later build keeps it up to date (see IDEs). |
| `anvil fmt [paths] [--check]` | Format C and C++ files with clang-format. See Formatting and checking. |
| `anvil tidy [targets] [--fix]` | Check the targets' sources with clang-tidy. |
| `anvil update [names]` | Move git dependencies to the newest commit of their branch or tag, and vcpkg to its newest ports; rewrite `anvil.lock`. `anvil update json` or `anvil update vcpkg` for one; an unknown name is an error. vcpkg's newest ports are those its checkout has (`git -C <vcpkg> pull` first). |
| `anvil schema [--tests] [--install]` | Print the JSON Schema of `anvil.toml` (or `tests.toml`) for editors. See IDEs. |
| `anvil completions <shell>` | Print a completion script: `bash`, `zsh`, `fish`, `powershell`, `elvish`. |

Build options. `build`, `run`, `test` and `install` take them all; `tidy` all but `--timings`; `targets`, `ide`, `compdb`, `update` and `convert --check` only the first five, which plan:

| Option | Meaning |
|---|---|
| `-r`, `--release` / `-p <name>` | Release profile, or a named profile. |
| `--toolchain <name>` | `msvc`, `clang-cl`, `clang` or `gcc` (default: the project's, else `anvil toolchains --set-default`'s, else the compiler `CXX` names, else the best installed). |
| `--target <platform>` | Build for another machine: `x86`, `arm64`, `x64`, `arm`, or a triple such as `aarch64-linux-gnu` (see Cross builds). A mistyped machine gets a did-you-mean; a target's name there is an error pointing at `anvil build <name>`. |
| `--locked` | Fail instead of changing `anvil.lock` (for CI). |
| `-o name=true\|false` | Set an option (`-o name`, `-o !name` work too). |
| `-j <n>`, `-k` | Parallel jobs (default: one per core plus two); keep going after a failure. |
| `-v`, `--explain` | Print every command; say why each action runs (which argument, file or output changed). |
| `--timings` | After the build, print the 10 slowest actions and the time per kind of action; with nothing to do, says so and names the last trace. |
| `--low-priority` | Run compilers at below-normal priority. |

Every command takes `-C <dir>` (run as if started in that folder, like `git -C`; `anvil run`'s program starts there too) and `-q` (no progress lines or notes; vcpkg's output only when it fails). `NO_COLOR` turns colors off; `CLICOLOR_FORCE` forces them on, also when the output is not a terminal.

Every build that runs something, failed and interrupted ones too, writes a Chrome trace of its actions to `out/<profile>/.anvil/trace.json` (open it in ui.perfetto.dev); a no-op build keeps the last one. Every build also writes the whole project's compile commands, sorted, to `out/<profile>/compile_commands.json` and `out/compile_commands.json`, also when it builds one target; the root's, once `anvil compdb` or `anvil ide` wrote it, follows them (see IDEs).

`CXX` (and `CC`) name the compiler when no toolchain is chosen, or the program of the chosen one when it is of that family. A compiler cache in front (`ccache g++`) is skipped; a `CXX` Anvil cannot use is reported and ignored.

## Conventions

With no `[target]` tables, a package builds from its layout:

- `src/` with a `main` file is a program; otherwise `src/` is a static library. With no `src/`, the `*.cpp` files beside `anvil.toml` form a program.
- `include/` holds public headers (passed on to dependents), `src/` is a private include folder.
- `tests/`: one test per file, or one test program from all of `tests/` when it has a `main`. Tests link the library and use `[dev-deps]`. A test named like the main target becomes `test_<name>`; another file's test also named `test_<name>` is then an error.
- Every `[deps]` entry is used by the main target.
- Output goes to `out/<profile>/` (`[project] out` changes the folder; it must be one of its own: the project's folder, one above it or a package's is an error). Targets of other packages build into `out/<profile>/packages/<package>/`; their programs and shared libraries still go to `out/<profile>/`.

Explicit targets use the same conventions: their sources default to `src/**/*.{c,cc,cpp,cxx,c++,cu}` under the target's `dir`, `include/` is public and `src/` private. An empty list (`sources = []`, `include = []`, `public.include = []`) turns a convention off. `include` and `public.include` in the target's own table replace the convention; in `[defaults]`, presets and `when` blocks they add to it.

Files a `when` block lists in `sources` belong to that block: they leave the default sources on every platform and come back only where the block holds. So `when.windows` with `sources = ["src/win/*.cpp"]` keeps `src/**` everywhere, minus `src/win/`, which only Windows compiles. `!pattern` removes files from the final list, also from a `when` block; `!folder` removes everything in it; a list of only `!` patterns is the convention minus those. A program or library that ends up with no sources (and no build steps or object libraries to supply them) is an error.

## anvil.toml

```toml
[project]
name = "game"
version = "1.4.0"        # {version}; one word ("1.4.0-beta.2"), no / \ " ;
std = "c++23"            # c++17 | c++20 (default) | c++23 | c++26
c-std = "gnu17"          # gnu17 (default) | c17 (strict ISO; also "17") | c11 | c23 (also c99, "gnu11")
toolchain = "msvc"       # default: the best installed
launcher = "sccache"     # compiler cache (ANVIL_LAUNCHER overrides)
default-run = "game"

[options]
tracy = false
editor = { default = true, description = "Build the editor" }

[deps]
fmt = "vcpkg"
sdl3 = { vcpkg = "sdl3", features = ["vulkan"] }
glm = { git = "https://github.com/g-truc/glm", tag = "1.0.1" }
engine = { path = "../engine" }
zlib = { pkg-config = "zlib", if = "linux" }
user32 = "system"
tracy = { vcpkg = "tracy", if = "tracy" }

[dev-deps]               # tests only
gtest = "vcpkg"

[defaults]               # every target
warnings = "high"

[preset.engine-module]   # targets opt in with preset = "engine-module"
werror = true
public.defines = ["ENGINE_MODULE"]

[target.core]
kind = "lib"             # exe | lib | shared | object | headers | test | custom
dir = "engine/core"
preset = "engine-module"
deps = ["sdl3", "glm", "engine", "zlib"]  # the target's own
defines = ["CORE_DATA=\"{root}/data\""]
pch = "src/pch.h"
unity = true             # or a number of files per unity source

[target.core.public]     # the target's and its dependents'
include = ["include"]    # (the default when include/ exists)
deps = ["fmt"]
defines = ["CORE_STATIC"]

[target.core.interface]  # its dependents' only
defines = ["CORE_API=__declspec(dllimport)"]

[target.core.when.windows]
deps = ["user32"]

[target.core.when."msvc & debug"]
flags = ["/JMC"]

[[target.core.file]]     # settings for some files
match = "src/simd/**/*.cpp"
arch = "avx2"

[[target.core.run]]      # a build step
for-each = "shaders/*.hlsl"
output = "{gen}/shaders/{stem}.h"
command = "dxc -T ps_6_0 {input} -Fh {output}"

[target.game]
kind = "exe"
deps = ["core", "tracy"]
gui = true               # WinMain; "main" for a GUI program with main()
data = ["assets/**"]     # copied beside the program

[target.game_tests]
kind = "test"
sources = ["tests/*.cpp"]
deps = ["core", "gtest"]
args = ["--gtest_brief=1"]

[[test]]                 # a test that runs a command
name = "smoke"
command = ["{target:game}", "--smoke"]

[[test]]                 # one test per level and size
name = "level_{level}_{size}"
command = "{target:game} --level {level} --window {size}"
matrix.level = ["town", "cave", { level = "sky", timeout = 600 }]
matrix.size = ["640x360", "1920x1080"]

[profile.shipping]
inherits = "release"
lto = true
static-runtime = true
defines = ["SHIPPING"]

[vcpkg]
linkage = "static"       # or "dynamic"; or triplet = "..."
baseline = "..."
overlay-ports = ["ports"]
overlay-triplets = ["triplets"]
```

### Target keys

| Key | Meaning |
|---|---|
| `kind` | `exe`, `lib` (static), `shared`, `object`, `headers`, `test`, `custom`. |
| `dir` | Folder the target's relative paths start from. |
| `if` | The target exists only when this condition holds. |
| `default` | Built by a plain `anvil build` (default true; tests false). |
| `preset` | A `[preset.<name>]` or a list of them, applied before the target's own settings. |
| `sources` | Globs; `!pattern` excludes. C, C++, CUDA (`.cu`), Objective-C (`.m`, `.mm`; clang and gcc), `.rc` (with `_DEBUG` under the MSVC ABI's debug runtime, as cl), and assembly (`.asm` with MSVC through ml64, `.s`/`.S` with GCC and Clang). `.def` and `.manifest` files go to the link (a manifest with the MSVC ABI only) and relink when they change; headers (`.h`, `.inl`, `.ipp`, ...) are ignored; C++20 module files (`.cppm`, `.ixx`) are an error. Objects link in the order the sources are listed, whatever their language (a glob's files sorted by path), generated sources after them, as CMake links them. |
| `include`, `system-include` | Include folders, the second with warnings silenced (default: `src/`). One that does not exist is warned about (not under the output folder, nor in a package with build steps). |
| `defines`, `flags`, `link-flags` | Definitions; raw compiler and linker settings. `{target:X}` in `link-flags` is X's file, linked first. |
| `deps` | Targets and `[deps]` entries this one uses; `pkg/target` names a target of a path or git dependency. |
| `whole-archive` | Static libraries (targets or `[deps]` entries) linked whole, every object in them, also those nothing calls: a program exporting an engine to the plugins it loads. They are linked as deps too. |
| `delay-load` | DLLs a program or shared library loads on its first call into them instead of at start (`["nvml.dll"]`): `/DELAYLOAD` with `delayimp.lib` (MSVC, clang-cl). A file name ending in `.dll`, each once; an entry the binary imports nothing from (misspelt, or its import library not linked) is an error. MinGW cannot delay-load: an error there (put it under `when.msvc`). Other systems ignore it. |
| `public` | A table of `include` (default: `include/`), `system-include`, `defines`, `flags` and `deps` for this target and every target using it. A public dep passes its own public settings on. |
| `interface` | A table of `include`, `defines` and `flags` for the targets using this one only (`FOO_API=__declspec(dllimport)`). |
| `libs`, `lib-dirs` | System or prebuilt libraries (`user32`, `pthread`, `Cocoa.framework`, a `.lib`/`.a` path; a folder is an error); folders to find them in. A library file that changes relinks the target. |
| `header-deps` | Targets whose include folders and definitions this one uses, without linking them. |
| `needs` | Targets built before this one; nothing of theirs is linked or included (CMake's `add_dependencies`). |
| `export-header` | Write an export macro header: `"mylib_export.h"` or `{ path = "...", macro = "MYLIB_API" }`. It goes under `{gen}`; a shared build defines `<NAME>_SHARED` for everyone and `<NAME>_BUILDING` for the library. |
| `exports` | A program that exports symbols for the modules it loads (an import library on Windows, `-rdynamic` on ELF); modules link against it. |
| `link-depends` | Files whose change relinks the target (a `.def` file, a linker script). |
| `output-name`, `output-prefix`, `output-suffix`, `output-dir` | The output file's name, prefix (`lib`), extension (`.dll`, `.mod`) and folder. |
| `version`, `soversion` | A shared library's version (`"1.2.3"`, `"{version}"`) and ABI version (`"1"`, default: `version`). On Linux the file is `libfoo.so.1.2.3`, known as `libfoo.so.1` (its SONAME), with the links `libfoo.so.1` and `libfoo.so`; on macOS `libfoo.1.2.3.dylib`, known as `libfoo.1.dylib`. Built and installed alike; a new version removes the files and links of the old one from the output folder. DLLs and other kinds ignore them. |
| `data` | Globs (relative to `dir`) copied beside the program or shared library, at their path under `dir`, when they change. From `[defaults]` and presets only programs, tests and shared libraries take it; a list of only `!` patterns is an error. |
| `install` | Whether `anvil install` takes it (default: a default target, less test-support libraries: those that reach `[dev-deps]` and that only tests use; tests never). |
| `pch`, `unity` | Precompiled header; unity builds (`true`, or files per unity source, 2 or more). |
| `warnings`, `werror` | `off`, `default`, `high`, `all`; warnings as errors. |
| `arch` | `avx`, `avx2`, `avx512`, `native`. |
| `cuda-arch`, `cuda-flags` | GPUs `.cu` files compile for, as CMake's `CUDA_ARCHITECTURES` reads them (`"native"`, `"all"`, `"86"` for code and PTX, `"86-real"`, `"86-virtual"`, a list or `"75;86"`); extra nvcc options. |
| `qt` | `false` turns Qt's moc, uic and rcc off for a target that uses Qt (default true). See Qt. |
| `std`, `c-std` | Per-target language standard. |
| `gui` | Windows GUI program: `true` for one entered through `WinMain` (CMake's `WIN32`), `"main"` for one with `main()`. A link that misses `WinMain` says so. |
| `link-language` | `"c++"` or `"c"`: the compiler driver that links it, as CMake's `LINKER_LANGUAGE` (default: C++ when it or a static library it links compiles C++, generated sources and C files compiled as C++ by a `-xc++`/`/TP` file rule included). Needed for a C program on a prebuilt C++ library in `libs`. On a library: what the binaries linking it count it as. MSVC ignores it. |
| `when.<condition>` | A table of the same keys (`kind` too: `when.SHARED_LIBS` with `kind = "shared"`; not in the `when` blocks of `[defaults]` and presets) that applies when the condition holds. |
| `file` | `[[...file]]` rules: `match`, `if`, `defines`, `flags`, `warnings`, `arch`, `opt`. A `-x <language>` (gcc, clang) or `/TP`/`/TC` (cl) in `flags` compiles the file as that language. |
| `run`, `pre-build`, `post-build` | Build steps (below). |
| `args`, `workdir`, `timeout`, `env` | Only for `kind = "test"` (and `[[test]]`). `timeout` is in seconds (default 600). |
| `will-fail`, `after`, `serial` | The test passes by failing; runs after other tests; runs alone. |
| `pass-regex`, `fail-regex`, `skip-exit-code`, `skip-regex` | Judge a test by its output; report it skipped. |
| `expect`, `reject`, `exit-code` | Regexes the output must each match; regexes it must not match; the exit code that passes (default 0). |

Settings are the target's own unless they are under `public` or `interface`; only `[deps]` of a package without `[target]` tables pass on to its users. The old `private-include`, `private-deps`, `public-defines`, `public-flags` and `interface-defines` keys are rejected with the key to write instead.

With gcc and clang outside the MSVC ABI, a program or shared library links through the C compiler, without the C++ runtime, unless C++ (CUDA and Objective-C++ too) is compiled into it or into a static or object library it links, or it links a vcpkg or `cmake` package's static library (C++ unless the package's `IMPORTED_LINK_INTERFACE_LANGUAGES` says C only); `libs` and pkg-config libraries count as C. `link-language` decides instead.

`c-std = "gnu17"`, the default, is C17 with GNU extensions: gcc and clang predefine `unix` and `linux` on Linux, so a struct member or variable of that name breaks; `c-std = "17"` (or `"c17"`) is strict ISO C, where glibc hides POSIX functions such as `strdup` unless a feature macro asks for them.

### Dependencies

A `[deps]` entry is `"vcpkg"`, `"pkg-config"`, `"cmake"`, `"system"`, or a table with one of `vcpkg` (with `features`, `default-features`), `git` (with one of `tag`, `branch`, `rev`; submodules are fetched too), `path`, `pkg-config` or `cmake` (with `components`, `version`, `targets`, `prefix`; see SDKs and installed libraries). A path or git dependency is an Anvil package, or a folder described by `include`, `sources` and `defines`; `libs`, `lib-dirs`, `flags`, `link-flags` and `runtime` (files copied beside the programs, never folders) describe vcpkg and prebuilt libraries. A folder with a `CMakeLists.txt` but no `anvil.toml` (and no `include` or `sources`) builds by the layout, with a warning suggesting `anvil convert` (for a git dependency, `include` and `sources` keys or converting a fork). `pkg-config` entries run `PKG_CONFIG`, else `<triple>-pkg-config` in a cross build, else `pkg-config` or `pkgconf`. Only the targets of a dependency that yours reach (or that tests and the command line name: `anvil run dep/tool`) are resolved; its `[[write]]` files are written. A dependency may declare `[options]` too, which `-o` sets, also while the dependency is off. Two packages of one name are an error. In a package with `[target]` tables, a `[deps]` or `[dev-deps]` entry nothing names (in `deps`, `public.deps`, `whole-archive`, `header-deps`, `needs` or `{target:...}`) gets a warning. A target's own name in its `deps` is the `[deps]` entry of that name (a program `threads` with `threads = "system"`).

`openmp`, `threads` and `opengl` = `"system"` are built in, as CMake's find modules: the compiler's OpenMP flag (LLVM's libomp for clang and clang-cl on Windows, MinGW gcc's `libgomp-1.dll` and the DLLs it loads, copied beside the programs and installed; Apple clang's `-Xclang -fopenmp` with Homebrew's `libomp` when it is installed, else `-lomp`), `-pthread` outside Windows, `opengl32`/`GL`/`OpenGL.framework`. With a condition: `openmp = { libs = ["openmp"], if = "..." }`. So are `vulkan = "system"`, the Vulkan SDK `VULKAN_SDK` names (on Windows its `Lib` for x64, `Lib32` for x86, `Lib-ARM64` for arm64; a missing one is an error naming it), else the system's `libvulkan` outside Windows, and `cuda = "system"`, the CUDA toolkit's headers, its static runtime and the driver library.

`[dev-deps]` takes the same entries, for tests only: a program, or a library that something other than tests uses, cannot name them. Every package's `[dev-deps]` are checked like its `[deps]`.

### Tests

`[[test]]` runs a command (`command`, a list of words or one line split like a shell's; `workdir`, `env`, `timeout`, `if`, `after`, `serial`, `will-fail`, the regexes). Tests go in `anvil.toml` or in a `tests.toml` beside what they test: Anvil reads every `tests.toml` in the package (only for `anvil test` and `anvil targets`), which holds `[[test]]` tables and nothing else, with the same placeholders and paths as `anvil.toml`. A test passes when it exits with `exit-code` (0 unless set), every `expect` regex matches its output (stdout and stderr together) and no `reject` regex does; a failure says which one, and what a `reject` matched. `pass-regex` and `fail-regex` keep CTest's meaning (a matching `pass-regex` passes whatever the exit code). A command that is `cmake -E ...` (`env` with `--unset` and `--modify`, `compare_files`, ...) is run by Anvil itself; `--modify` starts from the test's `env`.

```toml
[[test]]
name = "cpu_check"
command = ["{target:game}", "--headless"]
env = { GAME_TEST_CPU = "fail" }
expect = ["needs a processor with AVX2", "(?s)Missing: .*AVX2"]
reject = ["ERROR"]
exit-code = 1
```

Test names are unique in a package; a name two packages share becomes `<package>/<name>` outside the root package. `after` names its own package's tests first, and waits only for tests in the same run (not for one whose `if` is false); tests that wait for each other in a circle are an error. Processes a test leaves running when it ends go on running; at a timeout or Ctrl-C its whole process tree stops (an interrupted run exits with 130). On Linux and macOS, SIGTERM and SIGHUP to Anvil stop the running tests with what they started, and Anvil ends by the signal (exit code 128 + the signal); `anvil run` passes them to its program instead (without a terminal, Ctrl-C too, to its whole process group) and exits as it does. After a SIGKILL only the processes Anvil started directly end with it (Linux). A crash shows its code and meaning (`0xC0000005: access violation`; elsewhere the signal, `killed by SIGSEGV: segmentation fault`).

A `[[test]]` with a `matrix` is a template that stands for one test per combination of its axes, as nested loops would make them:

```toml
[[test]]
name = "stills_{path}_{view}"
command = ["python", "{root}/tools/stills.py", "--out", "{out}/{name}", "--view", "{view}", "--", "{path-args}"]
env = { PYTHONDONTWRITEBYTECODE = "1" }
matrix.path = [
    { path = "rt", path-args = [] },               # a table sets several parameters...
    { path = "raster", path-args = ["--no-rt"] },  # (a list fills whole items)
]
matrix.view = ["street", "dining", { view = "glass", timeout = 600 }]  # ...and test keys of its own
```

A text entry sets the parameter named after its axis; a table entry sets its keys, parameters or test keys (`name`, `after`, `timeout`, `expect`, ...) that replace the template's for its tests. `{param}` takes a parameter anywhere in the template's text, also inside another placeholder (`{target:{program}}`); a list parameter that is a whole item becomes that many items, and `{param:SEP}` joins it inside a word (`-DARGS={args:;}`). `name` is a pattern of the parameters, or each entry of one axis names its tests (`matrix.name = ["a", "b"]`); `{name}` is the test's name. A parameter nothing uses is an error, as are two entries setting the same key.

### Conditions

`if = "..."` keys and `when.<condition>` tables take expressions with `!`, `&`, `|` and parentheses, over these words:

- platform built for: `windows`, `linux`, `macos`, `unix`, `x64`, `x86`, `arm64`, `arm`, and `cross` (not this machine)
- compiler: `msvc` (MSVC ABI: cl and clang-cl), `gnu`, `cl`, `clang-cl`, `clang`, `gcc`, `mingw`
- profile: `debug`, `release` and every declared `[profile.X]` (a profile is also true under the profiles that inherit it)
- every `[options]` entry, and `true`, `false`

An unknown word is an error, not a silent false, also in a table this build does not use. The words are reserved: a profile or option named `windows` or `msvc`, or an option named like a profile (or `debug`, `release`), is an error, since the condition could not tell them apart.

### Placeholders

In paths, `defines`, `flags`, `link-flags` (of targets, file rules and profiles) and build steps:

| Placeholder | Value |
|---|---|
| `{root}`, `{out}` | The package root (in a profile, the package of the target being built); `out/<profile>`. |
| `{gen}`, `{name}` | The target's generated-files folder (`out/<profile>/generated/<target>`, on its include path); the target's name. |
| `{profile}`, `{config}` | The profile's name (`debug`, `rel-fast`); the same as CMake writes configurations (`Debug`, `RelWithDebInfo`). |
| `{version}` | `[project] version`. |
| `{vcpkg}` | The vcpkg triplet's install folder (`{vcpkg}/include/imgui.h`). |
| `{cxx}`, `{cc}` | The compilers' paths. |
| `{hash:a,b,...}` | 16 hex digits that change when what it lists does, computed at each build: a cache key in a define (`BVH_HASH=0x{hash:compiler,flags,src/bvh.cpp,{root}/external/bvh/*.h}ull`). Items are files and globs (relative to the target's `dir`), `compiler` (the compiler, its version and the machine built for) and `flags` (everything the target's files compile with: definitions, flags, standard, optimization, file rules; not include folders or the hashes themselves). A file that cannot be read or a glob that matches nothing is an error. In target `defines` and `flags`. |

In a package other than the root, `{target:X}` means that package's own X when it has one; `{target:pkg/X}` names another package's.

Paths (a target's `dir`, `include`, `sources`, `libs`, `lib-dirs`, `data`, `workdir`; a dependency's `path`, `include`, `lib-dirs`, `libs`, `runtime`, `prefix`) also take environment variables: `{env:NAME}`, `$NAME`, `${NAME}` and a leading `~`. An unset variable is an error, as is an empty one in a dependency's `include`, `lib-dirs`, `runtime` and `prefix` (the path would become another), and other placeholders there. A `prefix` that is one variable alone (`{env:CMAKE_PREFIX_PATH}`) is split as a list. A dependency's `defines`, `flags` and `link-flags` take `{env:NAME}` only.

Tests and `[[write]]` take `{root}`, `{out}`, `{profile}`, `{config}`, `{version}` and `{vcpkg}`; tests with a matrix also take their parameters and `{name}`. Build steps also take `{input}`, `{stem}`, `{filename}` (with `for-each`), `{inputs}`, `{output}`, `{outputs}`, `{depfile}`, and `{target:X}` (target X's file; also makes the step wait for X).

### Build steps

`[[target.X.run]]` needs `output` or `outputs`: every file the step writes. A step that succeeds without writing one fails. It re-runs when its command or inputs change (a file newly matching `inputs` counts), or when an output is missing or was edited since. `for-each` runs the command once per matching file. `inputs` adds files whose change re-runs it; files of the package its command names (`python {root}/tools/gen.py`) count too, and a step naming another step's output runs after it. `depfile = true` reads a make-style depfile from `{depfile}`. `shell = true` runs the line as written through `cmd /c` or `sh -c`, placeholders filled in and quoted when they hold spaces or shell characters. A command line longer than Windows allows is an error naming the step: give it fewer inputs, or use `for-each`. Generated sources (`.c`, `.cpp`, ...) are compiled into the target.

`[[target.X.pre-build]]` and `[[target.X.post-build]]` take `command`, `shell`, `message`, `workdir` and `if`, and have no outputs. A pre-build step runs before the target compiles, whenever its sources change. A post-build step runs after the file is made, and dependents wait for it. In every kind of step, `message` replaces the progress line, `workdir` is where it runs (default: the target's `dir`), `if` is a condition, and an empty `command` is an error.

Steps that run `cmake -E` (`copy`, `copy_if_different`, `copy_directory`, `copy_directory_if_different`, `make_directory`, `remove`, `rm`, `remove_directory`, `rename`, `touch`, `touch_nocreate`, `echo`, `echo_append`, `env`, `chdir`, `sleep`, `create_symlink`, `compare_files`, `cat`, `md5sum`, `sha1sum`, `sha224sum`, `sha256sum`, `sha384sum`, `sha512sum`, `true`, `false`), as converted CMake projects do, are run by Anvil itself, so no CMake needs to be installed; other `cmake` commands still run cmake.

`[[write]]` writes a file before the build (`path`, `text`, `if`), and rewrites it only when its text changes, so nothing rebuilds for nothing. It is what CMake's configure-time `file(WRITE)` and `configure_file` become. Two packages writing different text to one file is an error, and so is writing into a git dependency's checkout: write under `{out}`.

### Profiles

`debug` (the default) and `release` are built in. `[profile.X]` takes `inherits`, `opt` (`0`–`3`, `"s"`, `"z"`), `debug`, `lto`, `sanitize`, `debug-runtime`, `static-runtime`, `assertions` (false defines `NDEBUG`), `defines`, `flags`, `link-flags`, and `target` (the machine to build for, as `--target` takes it).

`static-runtime` links the C and C++ runtimes statically: the MSVC ABI's static runtime (`/MT`), `-static` with MinGW, `-static-libstdc++` in C++ links elsewhere; nothing on macOS, whose libc++ has no static form.

`sanitize` copies the sanitizer runtime DLL beside Windows programs. With clang and clang-cl on Windows it links the release C runtime (`debug-runtime` is ignored with a warning); MinGW gcc has no sanitizers, which is an error.

### Cross builds

```sh
anvil build --target x86                 # 32-bit Windows, from 64-bit Windows
anvil build --target arm64               # Windows on ARM
anvil build --target aarch64-linux-gnu   # with --toolchain clang or a GNU cross compiler on PATH
```

```toml
[profile.legacy32]
inherits = "release"
target = "x86"
```

- MSVC and clang-cl get Visual Studio's environment for that processor (`vcvarsall x64_arm64`); clang gets `--target=<triple>`; gcc uses `<triple>-g++` from `PATH` (or `-m32` for 32-bit on the same system, checked first: a gcc without multilib support fails clearly).
- A MinGW triple (`x86_64-w64-mingw32`) builds with gcc or clang; msvc and clang-cl refuse it.
- The vcpkg triplet follows (`x86-windows`, `arm64-windows`), and conditions see the target machine: `when.x86`, `when.arm64`, `when.cross`.
- `--target` builds into its own folder (`out/debug-arm64`); a profile's `target` uses the profile's folder.
- Programs that build steps run (`{target:gen}` in a `[[run]]` command) are built a second time for this machine, into `<out>/host`, so code generators work in a cross build.
- `anvil run` and `anvil test` refuse programs this machine cannot start (x64 Windows runs x86 programs; ARM Windows runs x64 ones too).

### Lock file

`anvil.lock`, beside `anvil.toml`, holds the exact versions a build used: each git dependency's commit, the vcpkg ports tree's commit (the `builtin-baseline` that fixes every port's version), and, for review, the port versions installed per triplet. Anvil writes it when it changes and prints what changed; commit it.

- A git dependency with a `branch` or `tag` stays on its locked commit until `anvil update` (or until `anvil.toml` names another branch, tag or `rev`).
- A pin stays while a loaded package declares it, also behind an `if` this build does not meet; a pin nothing declares goes.
- vcpkg stays on its locked baseline even when the vcpkg checkout moves on. With the project's own `vcpkg.json`, its `builtin-baseline` is used instead.
- `--locked` fails the build instead of changing the file, for CI: before fetching anything when a pin is missing, and when a declared git dependency's url or reference no longer matches its pin, even one this build does not use.

### Workspaces

```toml
[workspace]
members = ["engine", "tools/*", "game"]
```

Members are ordinary Anvil packages that depend on each other with `path` deps. Each member pattern must match a folder with an `anvil.toml`. Members may declare `[options]`; `-o` sets them. `[project]` `toolchain`, `launcher`, `out`, `default-run` and `export` apply to the whole workspace, from the root's manifest; in a member's they are an error. Run inside a member, Anvil builds the whole workspace; a workspace above a project its members do not list is ignored, even when it does not load.

## vcpkg

There is nothing to set up. Anvil looks for vcpkg in `VCPKG_ROOT`, on `PATH`, in Visual Studio installations and in common folders, then:

- installs the ports this build uses from `[deps]` (and the project's `vcpkg.json`, if there is one, which installs even with no vcpkg `[deps]`) into `out/vcpkg/installed/<triplet>`, skipping vcpkg entirely when nothing changed, and waiting when another vcpkg run holds its lock;
- with a `vcpkg.json` (`[vcpkg] manifest` names its folder when it is not the project's), checks that the features `[deps]` asks for are on in it (an entry whose `"platform"` holds, or the port's default features), turns on the manifest features that list a port `[deps]` uses only there, and accepts a port that a listed one brings in (`vulkan-headers` through `volk`); a port no target on this platform links may be missing;
- picks the triplet from the toolchain and profile (`x64-windows`, `x64-windows-static` with `static-runtime`; `x64-mingw-dynamic` for gcc on Windows, `x64-mingw-static` with `static-runtime`). `[vcpkg] linkage = "static"` or `"dynamic"` picks static libraries or DLLs for each machine and compiler (`x64-windows-static-md`). `[vcpkg] triplet` names one instead, which must fit the build, and whose C runtime must match the profile's `static-runtime` (a custom triplet's `VCPKG_CRT_LINKAGE` is read from its file); `triplet` and `linkage` together are an error;
- gives each port's users what its CMake package exports: include subfolders (`include/vma`), definitions (`FMT_SHARED`), compile and link options, and link-only system libraries, through the packages it links publicly, as CMake would, with generator expressions evaluated for the toolchain, the configuration (`Debug` exactly when debug libraries are linked) and each file's language; CMake's `OpenMP`, `Threads`, `OpenGL` and `Vulkan` targets become the matching flags and libraries;
- links each port's libraries (debug ones in debug builds), `lib/manual-link` libraries after them, and copies DLLs beside the programs;
- pins a `builtin-baseline` (the checkout's commit, or for Visual Studio's registry-only vcpkg the registry's) and keeps it in `anvil.lock`.

```sh
anvil vcpkg list         # every vcpkg found; * marks the one in use
anvil vcpkg use 2        # switch (a number from the list, or a folder)
anvil vcpkg add D:/vcpkg # register one in an unusual folder
anvil vcpkg setup        # download a private vcpkg, only if there is none
anvil vcpkg search sdl   # search ports
anvil vcpkg -- list      # run the selected vcpkg directly
```

The choice is stored per user in `settings.json` in Anvil's settings folder (`%LOCALAPPDATA%/anvil`; elsewhere `$XDG_CACHE_HOME/anvil` or `~/.cache/anvil`), beside the toolchain cache and the default toolchain. A project can pin one with `[vcpkg] root = "..."`.

## SDKs and installed libraries

Libraries installed outside vcpkg (an SDK's installer, a package manager, another project's `cmake --install` or `anvil install`) are `cmake` dependencies when they come with a CMake package:

```toml
[deps]
qt = { cmake = "Qt6", components = ["Core", "Widgets"], version = "6.5" }
fmt = "cmake"                                      # short for { cmake = "fmt" }
sdk = { cmake = "MySdk", prefix = "{env:MY_SDK}", targets = ["MySdk::core"] }
vulkan = "system"
cuda = "system"
```

A `cmake` entry links the imported targets of the package `find_package(<Package> CONFIG)` would find: in `prefix` (a folder or a list, searched in order, also for the packages it depends on), else `<Package>_DIR`, `<Package>_ROOT`, `CMAKE_PREFIX_PATH`, the prefixes of the folders on `PATH`, then the system's. `version` is the oldest that will do, judged by the package's version file; `components` link their targets (`Qt6::Widgets`, or the libraries of the package `Qt6Widgets` beside it); `targets` names the targets to link instead (default: the components', else the package's libraries). Users get what the targets give theirs, through the packages they name (`find_dependency`): include folders, definitions, compile and link options, libraries in link order, a debug build's own when the package has them. DLLs are copied beside the programs; elsewhere a shared library outside the system's folders gets an rpath. A package not found, or none that suits the build (version, pointer size, a missing component), is an error saying where Anvil looked and which variables would point at it. An MSVC-built C++ package in a MinGW build, or a MinGW one with MSVC, is an error: their C++ names differ. `anvil install` of a library using one writes a `find_dependency` for it, with the folder it was found in as a hint.

Anvil runs the package's CMake files as CMake would (variables, `if`, loops, macros and functions, `include`, `file(GLOB)`, `string` and `list`, `set_property`, the `target_*` commands); it never runs programs or writes files. A property built from a variable those files do not set themselves (a `find_library()` result, another package's variable) stops the build, naming the file, target, property and variable: describe such a package with `pkg-config` or with `include` and `libs`. Generator expressions see the profile's configuration (`debug` is `Debug`). A package's `cxx_std_NN` or `c_std_NN` raises its users' `std` or `c-std` when lower, as CMake does. vcpkg's packages are read the same way.

Targets of CMake's own find modules that packages link (`ZLIB::ZLIB`, `PNG::PNG`, `OpenSSL::SSL`, Qt's `WrapOpenGL::WrapOpenGL`) are looked for as those modules look: in the prefix of the package linking them, the dependency's `prefix` folders, the other packages' prefixes, `CMAKE_PREFIX_PATH`, then the system's; one found nowhere is left out with a warning. A package with only a find module (OpenSSL, PNG, ZLIB, Protobuf) is no `cmake` dependency: write `pkg-config = "<module>"` or `vcpkg`, as the error says.

What a lookup found is kept in `out/<profile>/.anvil/cmake-packages/` until a file it read, a folder it searched, an environment variable it consulted, the entry or Anvil changes.

`vulkan = "system"` and `cuda = "system"` need no package: see Dependencies. Libraries with neither a CMake package nor a pkg-config file are described with `include`, `libs`, `lib-dirs` and `runtime`, with `{env:NAME}` for where they are installed (`include = ["{env:MY_SDK}/include"]`).

## Converting from CMake

```sh
anvil convert path/to/project --stdout   # preview
anvil convert path/to/project            # write anvil.toml
```

The converter runs the CMake language itself (variables, functions, macros, `if`, `foreach`, `include`, `add_subdirectory`, `list`, `string`, `file(GLOB)`, `cmake_parse_arguments`, ...) and keeps track of which values depend on the platform, compiler and options. Then it:

- writes targets, their includes, defines, flags and dependencies, with `when` blocks for platform- and option-dependent settings (`kind`, `pch`, `std`, `gui` and the output name too); generator expressions become conditions, and a dependency used only under a condition gets `if`;
- leaves out what the conventions already cover, and turns long file lists back into globs;
- maps compiler flags to Anvil settings (`/W4` and `-Wall -Wextra` to `warnings = "high"`, `/arch:AVX2` and `-mavx2` to `arch = "avx2"`, ...); flags for one language (`$<COMPILE_LANGUAGE:C>`, `CMAKE_C_FLAGS`) become `[[file]]` rules by extension, or plain flags when the target has one language; C standards become `c-std`;
- maps `find_package`, `FetchContent` and CPM dependencies to vcpkg ports, git deps or pkg-config (`OpenMP`, `Threads` and `OpenGL` to `openmp`, `threads` and `opengl` = `"system"`); a `find_package` without `REQUIRED` finds what Anvil has (`OpenMP`, `Threads`, `OpenGL`, ports in `vcpkg.json`, FetchContent and CPM); one with a vcpkg port becomes an option `<Pkg>_FOUND`, off as on a machine without it, that adds the port; the absent ones, and programs `find_program` did not find, are reported; old-style `<PKG>_LIBRARIES` and `<PKG>_INCLUDE_DIRS` map to the dependency; a `find_package` no vcpkg port stands for becomes a `cmake` dependency with its components and version; `Threads` also sets `CMAKE_USE_PTHREADS_INIT` (`!msvc`) and `CMAKE_USE_WIN32_THREADS_INIT` (`windows`), as FindThreads does; `pkg_check_modules` without `REQUIRED` finds only modules this machine's pkg-config knows (`<PREFIX>_FOUND` is false otherwise, and reported); `find_program` keeps the name it found (a path only when just its `PATHS`/`HINTS` have it);
- turns `check_*` results (`check_include_file`, ...) into conditions (`!msvc`, `!windows`) instead of this machine's answer, and keeps a `cmake_dependent_option`'s dependencies in the option's condition; results are cache entries, seen outside the function that checks;
- converts `add_custom_command` into build steps (`PRE_BUILD`, `PRE_LINK` and `POST_BUILD` into pre- and post-build steps; `cmake -E echo` into a file becomes `[[write]]`, shell operators `shell = true`; a step running a program `find_program` did not find is dropped with a warning), and `add_dependencies` into `needs`;
- converts `configure_file`, `file(CONFIGURE)`, and configure-time `file(WRITE/GENERATE)` with known text into `[[write]]`, with `@VAR@`, `${VAR}`, `#cmakedefine` and `#cmakedefine01` applied: platform-dependent results (`check_type_size()` sizes too) become `#if` blocks in C and C++ files, option-dependent ones one `[[write]]` per variant; large unchanged copies become a copy step, and install-only files are skipped;
- converts `add_test`, `gtest_discover_tests` and test properties (`WILL_FAIL`, `DEPENDS`, fixtures, `RESOURCE_LOCK`, `WORKING_DIRECTORY`, regexes, ...) into tests, the regexes translated from CMake's syntax (braces and `\x` are literal, `.` matches line ends; one Rust cannot read matches as plain text, with a warning); a program run by its path in CMake's build folder (`./name`, `${CMAKE_CURRENT_BINARY_DIR}/name`) is that target, and `LABELS` are dropped. A test that runs a CMake script whose only work is to run one command and check its exit code and output against regex lists (`cmake -DCOMMAND=... -DEXPECT=... -P run_and_expect.cmake`) becomes a test of that command with `expect`, `reject` and `exit-code`, so it runs without CMake; a script that does anything more (writes files, runs more, reads other variables) stays a `cmake -P` test. Tests go to a `tests.toml` in the folder whose `CMakeLists.txt` adds them (the top folder's stay in `anvil.toml`). Tests of one kind (one helper function, or one program and script) become one `[[test]]` with a `matrix` when that is clearly shorter and every entry fits a line: what the tests share stays in the template, nested `foreach` loops come back as axes, and parameters are named after the loop variables, the helper's arguments or the options they follow (`-DSCRATCH=`, `--view`);
- keeps CMake's GNU extensions: `std = "gnu++17"`, `c-std = "gnu11"` unless `CXX_EXTENSIONS`/`C_EXTENSIONS` are `OFF`; `check_*_flag` passes GCC's warnings with GCC and Clang, Clang's with Clang, unknown flags nowhere (reported); a check's source passes only where its `#if` tests of platform macros let it compile; a target named like a built-in dependency (`threads`) is renamed `<name>-<kind>`, keeping its file name;
- writes `public` and `interface` sub-tables for `PUBLIC` and `INTERFACE` settings, and `PRIVATE` ones as the target's own;
- turns `file(SHA256 ...)` and `string(SHA256 ...)` of such hashes into `{hash:...}`, so the value follows the files instead of freezing at conversion: a hashed compiler name (`${CMAKE_CXX_COMPILER_ID}`) becomes `compiler`, a hashed `CMakeLists.txt` or `.cmake` file (hashed for the flags it sets) becomes `flags`, and files a `file(GLOB)` found become its pattern; other values only CMake's configure step knows (`file(TIMESTAMP)`, `string(RANDOM)`) are left out of defines and flags with a `# TODO`;
- writes paths without `dir/..` steps (`{root}/cmake/../docs` becomes `{root}/docs`);
- converts `generate_export_header` into `export-header` (or a header written into the project), an option `BUILD_SHARED_LIBS` into `when.BUILD_SHARED_LIBS` with `kind = "shared"`, and `DEFINE_SYMBOL` (or `<target>_EXPORTS`, when the code uses it) into a definition while the library is shared;
- reads vcpkg settings (triplet, overlay ports and triplets) from `CMakePresets.json`;
- turns `CMAKE_MSVC_RUNTIME_LIBRARY` into `static-runtime` and `debug-runtime` in `[profile.debug]` and `[profile.release]`, `HEADER_FILE_ONLY` set under a condition into sources under the opposite one, and custom targets without `ALL` into `default = false`;
- turns `CMAKE_SIZEOF_VOID_P` and `CMAKE_SYSTEM_PROCESSOR` tests into `x64`/`x86`/`arm64` conditions, and CUDA (`LANGUAGES CUDA`, `CUDA_ARCHITECTURES`, `CUDA::` libraries) into `.cu` sources with `cuda-arch`;
- moves settings every target shares into `[defaults]`, settings most targets share into `[preset.common]`, and settings the targets of one helper function share into a preset named after it (`my_add_plugin()` -> `[preset.my-add-plugin]`);
- turns `-Wl,--whole-archive ... -Wl,--no-whole-archive` and `/WHOLEARCHIVE:` into `whole-archive`, `/DELAYLOAD:x` link options of programs and shared libraries (also through `LINKER:`/`-Wl,`, and lld's `-delayload`) into `delay-load = ["x"]`, dropping `delayimp` from their libraries, `/ENTRY:mainCRTStartup` into `gui = "main"`, and a `for-each` over what a `file(GLOB)` found back into its pattern;
- turns `C_VISIBILITY_PRESET`, `CXX_VISIBILITY_PRESET` and `VISIBILITY_INLINES_HIDDEN` (and their `CMAKE_` variables) into gcc's and clang's flags, `CMAKE_INTERPROCEDURAL_OPTIMIZATION` into `lto` in the profiles (a target's own `INTERPROCEDURAL_OPTIMIZATION` is a TODO note), a shared library's `VERSION` and `SOVERSION` into `version` and `soversion`, and `LINKER_LANGUAGE` into `link-language`;
- writes commands as one line when they fit and no word needs quoting, and packs long lists into full lines.

Whatever it cannot convert (C++ module file sets, `WINDOWS_EXPORT_ALL_SYMBOLS`, unknown generator expressions, ...) is marked with a `# TODO` comment or reported as a warning. A result that does not parse, uses a condition word it does not declare, or holds a value the converter lost track of is refused as a converter bug instead of written; its targets, files and dependencies are checked when Anvil loads it.

### Checking a conversion

```sh
anvil convert --check                        # with the default options
anvil convert --check -o BUILD_TESTS=true    # or others
```

`--check` configures the CMake project with Ninja, the same compiler, the options given with `-o` (the others keep CMake's defaults) and vcpkg packages (into `out/convert-check`, started over when the compiler changes), plans the Anvil build without running it, and compares the two `compile_commands.json` file by file (`.rc` files too). It lists the files only one side compiles, and for the rest each difference in definitions, include folders, language standard and other compile options, grouped by what differs; what each side adds by itself (optimization, debug information, warning level, output and dependency files) is left out; MSVC's runtime (`/MD`, `/MT`, ...) is compared. It exits with 1 when anything differs. It needs `cmake` and `ninja`.

Limitations: `if` branches that depend on the platform or an option all run, each one's settings, values and appended items kept under its condition (`PARENT_SCOPE` and cache variables too); values a branch sets per platform, compiler or option stay per condition through `string()`, `math()`, `list()` reads, copies, `if(EXISTS)` and configured files (past 32 combinations in one text the converter uses this machine's value and warns). But a value only one branch replaces keeps this machine's (not a truth value, `ON`, `false`, `1`, ...: that is kept per condition), a `break()` under a condition ends a `foreach` only where it holds, an option declared with different defaults per platform takes this machine's default with a warning (Anvil options have one default), and names, paths and text read at configure time (`include`, `find_package`, `file`, `execute_process`, ...) take this machine's branch. CMake scripts run by build steps and tests (`cmake -P`), other than the check scripts above, still need cmake on `PATH` (`cmake -E` does not). Values computed at configure time from `execute_process` output cannot be converted; the converter says where they are.

## CUDA

`.cu` files compile with nvcc (from `CUDA_PATH`, `PATH`, or the newest toolkit installed), with the build's runtime library, debug information and definitions; their headers are tracked like any other. The static CUDA runtime links into whatever uses them, and the toolkit's headers and libraries (`libs = ["cublas"]`) are on their users' paths. nvcc accepts only the Visual Studio versions its release knows, so when the build's own is newer, Anvil hands nvcc the newest installed one it accepts (found once and cached; a toolkit that accepts none is remembered too, until nvcc, the compiler or the installed Visual Studios change). Each `.cu` file compiles whole: separable compilation (device code linked across files) is not supported, and CUDA does not cross-build.

## Qt

A target that uses Qt (`qt = { cmake = "Qt6", components = ["Widgets"] }`, or Qt 5, or a vcpkg `qt*` port, among its `deps`) gets Qt's code generators as CMake's AUTOMOC, AUTOUIC and AUTORCC run them, with no generate step: moc over its headers and sources with `Q_OBJECT`, `Q_GADGET` or `Q_NAMESPACE` (a header's `moc_<name>.cpp` compiles into the target; a source with the macro includes `"<name>.moc"` at its end, or `"<folder>/<name>.moc"`, which moc writes there; two sources including one `.moc` path are an error), uic over its `.ui` forms (`ui_<name>.h`), rcc over its `.qrc` files (`qrc_<name>.cpp`, named for `Q_INIT_RESOURCE(<name>)`). The headers moc reads are those in `sources`, `<name>.h` and `<name>_p.h` beside each source, and those of the target's own include folders named like a source; with the layout convention, also every other header of its own include folders and `src/` that has a moc macro, except in folders whose sources a `when` block or `!` pattern left out. `.ui` and `.qrc` files are sources (the convention finds them beside the C++ ones; in a target that does not use Qt they are an error); a form whose header a source includes is also found beside that source. Each tool runs as a build step with the files it read, so an edit reruns only what it affects; moc sees the target's definitions and include folders. What they write goes to `out/<profile>/qt/<target>/`, whose `include/` is on the target's include path and in `compile_commands.json`. The tools come from the Qt package (`Qt6::moc` in Qt6CoreTools, Qt 5's extras), else the installation's `libexec/` or `bin/`. `qt = false` on a target turns all three off (for a project that runs them in its own build steps).

On Windows, programs get the Qt DLLs they link and, beside them, the plugins Qt loads by default for those modules (`platforms/qwindows.dll`, `styles/`, `imageformats/`, ...), in the DLLs' configuration; a program that loads Qt Quick gets a `qt.conf` pointing at the installation's `qml/` folder (unless its `data` has one). `anvil install` copies them too. windeployqt is not run. Elsewhere Qt finds its plugins where it is installed. Qt Quick works with QML bundled in `.qrc` files; `qt_add_qml_module` (qmlcachegen, type registration, QML modules' plugins) is not supported: register C++ types with `qmlRegisterType` or context properties. macOS bundles are not supported.

`anvil convert` reads AUTOMOC/AUTOUIC/AUTORCC (on: nothing to write; off on a target: `qt = false`), `qt_add_executable` and `qt_add_library` (shared without a kind, as Qt is), `qt_wrap_ui`, `qt_wrap_cpp` and `qt_add_resources(VAR files.qrc)` (their files become sources), `find_package(QT NAMES Qt6 Qt5)` (Qt 6), versionless `Qt::` targets, and gives a WIN32 Qt program `gui = "main"`. Qt 6's `qt_add_resources(target name FILES ...)`, `qt_add_qml_module` and `qt_disable_unicode_defines` become TODO notes (undefine with `flags = ["-UUNICODE", "-U_UNICODE"]` in `when.windows`).

## Linux and macOS

Programs and shared libraries get the rpaths `$ORIGIN` and `$ORIGIN/../lib` (`@loader_path` on macOS), so they run from `out/`, from a copy of it and from an install prefix; a shared library from outside the project (pkg-config, prebuilt, a dynamic vcpkg triplet, a `cmake` package) gets an rpath to its folder unless the loader searches it anyway. `anvil run` says which signal ended a program (`killed by SIGSEGV: segmentation fault`) and exits with 128 + the signal, as a shell does. On Linux, the programs and tests Anvil runs end when Anvil is killed (see Tests for SIGTERM and SIGHUP).

On macOS, `g++` is Apple clang under another name: use the clang toolchain, or install GCC (Homebrew's `g++-15`) and name it in `CXX`. macOS is untested apart from CI (see Building Anvil): no Mac was at hand.

## Installing

```sh
anvil install                            # every default program and library, release, into out/install
anvil install -p debug                   # beside it, under debug/ (MSVC's debug runtime)
anvil install core --prefix D:/sdk/game
```

`anvil install` builds, then copies into the prefix, each file only when it changed:

- `bin/`: programs and DLLs, with the DLLs Anvil copies beside them (their dependencies', the sanitizer's, Qt's plugins and `qt.conf`), their `data` files, and their PDBs when the profile has debug information;
- `lib/`: static and import libraries, `.so` and `.dylib` files (a versioned one with its links, `libfoo.so.1` and `libfoo.so`);
- `include/`: each library's public include folders, and the headers its `export-header` and build steps generate; a header two of them hold installs from the first, as compiling finds it (zlib's generated `zconf.h` before the one in its sources). A folder of its own installs whole (less source files; a folder of sources such as `src/` only its headers); a folder holding the rest of the package (its root, the target's `dir`) gives its headers (extensionless ones too, `Eigen/Core`), those of the library's own source folders included (`mylib/mylib.h` beside `mylib/mylib.cpp`) unless they are private include folders (`include = ["src"]`, the default `src/`), leaving out other targets' source folders, private include folders, `tests/`, the output folder and the prefix. Folders outside the project stay where they are, named by path. A library that installs no headers gets a warning.

The libraries the installed ones need come along; tests, and libraries only tests use that reach `[dev-deps]`, are left out; a target's `install` key overrides the default. Linux and macOS programs find their libraries in `../lib`. For the libraries it writes `lib/cmake/<project>/<project>Config.cmake` with a `<project>Targets-<profile>.cmake` per configuration (and `<project>ConfigVersion.cmake` when `[project] version` is set, which then must be one to four numbers: checked before building), and `lib/pkgconfig/<project>-<library>.pc` per library plus `<project>.pc` for all; `[project] export = false` leaves them out (and removes them). The package describes every library whose files this configuration's install holds (`anvil install core` keeps the others); profiles with the same C runtime replace each other's `Targets-<profile>.cmake`. `openmp`, `threads`, `opengl` and `vulkan` reach users through `find_dependency(OpenMP)` (`Threads`, `OpenGL`, `Vulkan`) and the `.pc` files' flags, and `cmake` dependencies through `find_package` with their components and the folder they were found in as a hint; a usage that differs by configuration is written under `$<CONFIG:...>`. Libraries that use vcpkg ports refer to them in `out/vcpkg`, which is not copied (Anvil warns).

A profile with the debug runtime installs its programs and libraries under `debug/` (`debug/bin`, `debug/lib`), as vcpkg does, so a release and a debug install share one prefix, headers and CMake package. `share/<project>/install_manifest.txt` lists each configuration's files with the target that installed them, and the next install of the same targets removes those no longer installed (a removed target's too). A file that was in the prefix before (another project's, the user's) is overwritten with a warning but never removed. A prefix holding the project or its workspace is an error. With MSVC, a CMake Debug build of a consumer links the debug install and other configurations the release one; when the one it needs is missing, `find_package` fails with a message saying so instead of mixing runtimes, and a multi-configuration generator (Visual Studio) warns and fails to link that configuration, naming the missing files.

## Compiler cache

`[project] launcher = "sccache"` (or `"ccache"`, or a path) runs every C and C++ compile through a compiler cache. The `ANVIL_LAUNCHER` environment variable overrides it, for one machine or CI; set empty, it turns the cache off. A launcher that is not there is an error, not a quietly slow build; only commands that compile look for it (`targets`, `compdb`, `tidy` work without). A compile whose command line passes 30,000 characters runs without it on Windows (`-v` says so), as do cl's precompiled header compiles with sccache. ccache before 4.10 caches nothing for cl: Anvil warns. It is not part of the command line: turning it on or off rebuilds nothing, and `compile_commands.json` leaves it out.

## Formatting and checking

`anvil fmt` formats the packages' C and C++ files (or the `paths` given) with clang-format, as their `.clang-format` says; a file with none in its folder or above is an error, since clang-format would rewrite it in its own style. `.clang-format-ignore` files are read as the clang-format found reads them (up to 22 the first pattern that decides wins, so a lone `!` pattern leaves out every file it does not match; from 23 the last match wins), and output folders skipped. Files go through as bytes, in any encoding and line ending; an empty result for a non-empty file is an error, never written. `--check` changes nothing: it lists the files that are not formatted and exits with 1 when there are any.

`anvil tidy` runs clang-tidy (as the project's `.clang-tidy` says) over the targets' sources with the profile's compile commands, for cl and GNU toolchains alike, a precompiled header replaced by its header; the build steps the sources need (generated headers) run first. A warning in a header shows once; headers are checked too (`--header-filter=.*`) unless the `.clang-tidy` sets `HeaderFilterRegex`. `--fix` applies the fixes. It exits with 1 when it reports anything.

Both find their tool on `PATH`, in Visual Studio's `VC/Tools/Llvm`, or in LLVM's install folder.

## IDEs

`anvil ide [vs|vscode|clion|zed|all]` writes an IDE's project files into the project. They hold no build logic: building runs `anvil build`, a debugger starts the program as `anvil run` would (its file, arguments, folder and environment), and IntelliSense reads `compile_commands.json`. With no IDE named, it refreshes those set up before and those whose folders are there. It takes the planning options: the files' builds use the profile, toolchain, machine and options it was given. `all` is every IDE (Visual Studio on Windows only).

| IDE | Files | What they do |
|---|---|---|
| `vs` | `<project>.sln` at the root; a Makefile project per target in `.anvil/vs/` | Build, Rebuild and Clean of a project run `anvil build <target>` and `anvil clean <target>` (Rebuild cleans first); F5 debugs the startup project (`default-run`, else the first program) with Visual Studio's debugger. A configuration per profile that builds for Windows (`Debug`, `Release`, `Shipping`), each described by its own profile (its targets, programs, arguments and IntelliSense), so the files are the same whichever profile builds. IntelliSense takes each target's include folders, definitions and standard from that profile's `compile_commands.json`, once a build of it wrote one. A workspace gets a solution folder per package. |
| `vscode` | `.vscode/tasks.json`, `launch.json`, `settings.json` | Tasks `anvil: build` (the default build task), `anvil: build <program>` and `anvil: test` (the default test task), with problem matchers for cl and GCC; a launch configuration `anvil: <program>` per program, built first (`cppvsdbg` for the MSVC ABI, else `cppdbg` with gdb or lldb); settings pointing cpptools at the root `compile_commands.json` and, for gcc and clang, clangd's `--query-driver` at the compiler (each unless set), and Even Better TOML at Anvil's schemas. |
| `clion` | `.idea/tools/Anvil.xml`, `.idea/customTargets.xml`, `.idea/runConfigurations/anvil_<program>.xml`, `.idea/misc.xml` (when there is none) | A compilation database project (the root `compile_commands.json`) with custom build targets that run anvil, and a Custom Build Application run configuration per program. |
| `zed` | `.zed/tasks.json`, `.zed/debug.json` | Tasks `anvil: build` and `anvil: test`; a debug configuration per program, built first (GDB's adapter for gdb, else CodeLLDB). |

Programs include test targets, which run as their tests do. CLion's and Zed's files are written to their formats but untested: no CLion or Zed was at hand.

In files the user edits too (VS Code's and Zed's lists, CLion's `customTargets.xml`), Anvil writes its entries, named `anvil: ...`, and replaces or removes only those and the ones it wrote before, which `.anvil/ide.toml` lists; every other entry, and every comment, stays. A file it cannot read as UTF-8 stays as it is, with a warning saying what Anvil would add (the next build after it is fixed adds it). A byte-order mark and CRLF line endings are kept.

Nothing goes stale: every build refreshes the files `anvil ide` set up (recorded in `.anvil/ide.toml`) when what they describe changed (targets, programs, profiles, manifests, the anvil writing them), writing only the files whose text changes, so the IDE reloads nothing for nothing. Builds with the toolchain, machine and options `anvil ide` was given refresh them: those of its profile, and for Visual Studio, whose files describe every profile, those of any. Deleting an IDE's folder (or the solution) stops its refreshes; a file that cannot be refreshed is a warning, never a failed build. The files run `anvil` when the one that wrote them is on `PATH`, else its path.

`anvil ide` and `anvil compdb` write `compile_commands.json` at the project root, where clangd and cpptools find it, and every later build keeps it the same as its own. Once IDE files exist, only builds of the profile, toolchain, machine and options `anvil ide` was given update it: run `anvil ide` with others to switch. It always covers the whole project, sorted, `.rc` files included. `anvil new` and `anvil init` leave it, `.anvil/`, `.vs/` and the solution out of git; `anvil ide` says what to add to an existing `.gitignore`.

### anvil targets --json

`anvil targets --json` prints the project as an editor extension reads it, on stdout alone (warnings go to stderr). It takes the planning options and builds, fetches, installs and writes nothing; a manifest error exits with 1 and the usual message. The shape is version 1: fields may be added, never renamed. Paths are absolute, with forward slashes.

| Field | Value |
|---|---|
| `version` | `1`. |
| `root`, `name` | The project's folder; the root package's name. |
| `profile`, `profiles` | The profile described; `debug`, `release` and every `[profile.X]` of the root manifest. |
| `toolchain` | `family` (`msvc`, `clang-cl`, `clang`, `gcc`), `version`, `abi` (`msvc` or `gnu`), `platform` (`x64-windows`, `arm64-linux`, ...), `debugger` (`msvc`, `gdb`, `lldb`) and `debugger_path` (null when not found, and for `msvc`). |
| `out_dir`, `compile_commands` | The profile's output folder; `out/compile_commands.json`. |
| `default_run` | The program `anvil run` starts when none is named, or null. |
| `targets` | The project's targets (workspace members' too): `name`, `package`, `qualified` (`pkg/name` outside the root package), `kind`, `default`, `runnable` (a program this machine can start), `output` (its file, or null), `workdir`, `args` and `env` (as `anvil run` starts it; null, `[]` and `{}` for libraries), `dir`, `gui`. |
| `tests` | Every test: `name`, `program` (the target its command starts with, or null), `command` (`{target:X}` replaced by X's file), `workdir`, `env`. |

### VS Code

The Anvil extension in `editors/vscode` needs no generated files: open a folder with an `anvil.toml` and press F5. It reads the project from `anvil targets --json` and gets everything about `anvil.toml` and `tests.toml` from the language server, so it never disagrees with `anvil build`:

- F5 builds the program chosen in the status bar and debugs it with the C/C++ extension (`cppvsdbg` for MSVC and clang-cl builds, else `cppdbg` with gdb or lldb), as `anvil run` starts it; Ctrl+F5 runs it in a terminal. Run and Debug lists a configuration per program and per test that runs one; `launch.json` takes `{ "type": "anvil", "request": "launch", "target": "game" }` (or `"test": "smoke"`).
- Tasks (type `anvil`): build (Ctrl+Shift+B), test, build or run a target, clean; compiler errors go to Problems.
- Test Explorer: the tests of `anvil.toml` and every `tests.toml`, run through `anvil test`; debugging one debugs its program with the test's arguments and folder.
- Status bar: the profile, the toolchain and the program to run.
- IntelliSense: clangd and the C/C++ extension pointed at `out/compile_commands.json`, unless set.
- `anvil.toml` and `tests.toml` are *Anvil TOML*, with the extension's own grammar: no TOML extension is needed, and Even Better TOML's schema does not apply.

```sh
cd editors/vscode
npm install
npm run package                            # anvil-0.1.0.vsix
code --install-extension anvil-0.1.0.vsix
```

It needs `anvil` on `PATH`, in `~/.cargo/bin` or named by the `anvil.path` setting, and the C/C++ extension (`ms-vscode.cpptools`) for debugging. `editors/vscode/README.md` lists its settings and how to run its tests. With it, `anvil ide vscode` is not needed.

### Other editors

`anvil schema` prints the JSON Schema of `anvil.toml` (`--tests`: of `tests.toml`) for editors that read one (Taplo, Even Better TOML in VS Code, JetBrains' TOML plugin): completion, hover text, and unknown keys underlined. `anvil schema --install` writes both to `%LOCALAPPDATA%/anvil` and prints the `evenBetterToml.schema.associations` setting for VS Code; a `#:schema <path>` first line points one file at it. A project can commit its own with `anvil schema > anvil.schema.json`. The Anvil VS Code extension does not need this: its language server, `anvil lsp`, does it.

`anvil completions <shell>` prints a shell completion script (`anvil completions powershell >> $PROFILE`).

## Language server

`anvil lsp` is a language server (LSP 3.17 over stdin and stdout) for `anvil.toml` and `tests.toml`; it ignores other files, so attaching it to every TOML file is harmless. It runs the load `anvil targets` runs (nothing fetched, installed, built or written) on the editor's unsaved text, 200 ms after the last edit and at once on save, so every mistake a build would stop at shows before anything builds: TOML syntax, unknown keys (with did-you-mean), wrong value types, condition words, targets, packages, profiles, options, presets, tests and their `after`, cycles, duplicate names, workspace members, `[deps]` entries; and as warnings unused dependencies, missing include folders, globs that match nothing, path dependencies with only a `CMakeLists.txt`. Diagnostics mark the key or value at fault, in the file that holds it (a path dependency's manifest, a `tests.toml`), and every error of a check shows, not only the first. The editor pulls them when it can, else the server pushes them.

It completes keys, values, condition words, target and dependency names, placeholders and vcpkg ports (with snippets for tables and build steps where the editor takes them); shows each key's documentation and what a name is on hover (Markdown when the editor offers it, else plain text); goes to definitions, finds references and renames targets, `[deps]` entries, options, profiles, presets and tests across files; lists symbols; colours what Anvil gives a meaning (condition words, placeholders, target names); and offers quick fixes: the did-you-mean, removing an unused dependency, adding a vcpkg port to `[deps]`.

Settings, in `initializationOptions` or the editor's `anvil` settings: `profile` and `toolchain` (the load it checks; default: the project's), and `trace` (`true`: log what it does to stderr and to the editor's log, as `anvil lsp --verbose` does; so does the protocol's own `verbose` trace).

- VS Code: the Anvil extension starts it.
- Neovim 0.11+: `vim.lsp.config('anvil', { cmd = { 'anvil', 'lsp' }, filetypes = { 'toml' }, root_markers = { 'anvil.toml' } })` and `vim.lsp.enable('anvil')`.
- Helix (`languages.toml`): `[language-server.anvil]` with `command = "anvil"` and `args = ["lsp"]`, and under `[[language]]` `name = "toml"`, `language-servers = ["taplo", "anvil"]`.
- Emacs: `(add-to-list 'eglot-server-programs '((toml-ts-mode conf-toml-mode) . ("anvil" "lsp")))` (eglot runs one server per mode, so this replaces taplo).
- Sublime Text (LSP package): `"clients": { "anvil": { "enabled": true, "command": ["anvil", "lsp"], "selector": "source.toml" } }`.
- Zed: a small extension whose `extension.toml` has `[language_servers.anvil]` with `name = "Anvil"` and `languages = ["TOML"]`, and whose `language_server_command` returns `anvil` with `["lsp"]`.
- Drift: command `["anvil", "lsp"]`, extensions `["anvil.toml", "tests.toml"]`, language `"toml"`.

## Speed

A generated project with 500 source files and 500 headers, on 16 cores, MSVC debug build:

| | CMake + Ninja | Anvil |
|---|---|---|
| Configure | 2.0–2.9 s | none |
| Full build | 25.0–27.4 s | 24.9–26.9 s |
| No-op build | 32–48 ms (16 ms with `ninja` directly) | 35–40 ms |
| Touch a header, content unchanged | 0.60 s | 0.035 s |
| Edit a header used by 2 files and `main.cpp` | 0.60 s | 0.55 s |

At scale, 20 static libraries of 500 files each plus a program (10,001 files), same machine:

| | CMake + Ninja | Anvil |
|---|---|---|
| Configure | 5 s | none |
| Full build | 68–75 s | 62–73 s |
| No-op build | 172–191 ms (151–169 ms with `ninja` directly) | 184–196 ms |

The no-op includes everything CMake's configure step would otherwise do: parsing the manifest and planning
10,022 actions. Full builds on this machine vary by several seconds run to run; Ninja running Anvil's own
compile commands took 60–68 s, so what is left of the difference is Anvil hashing what each compile read.

Why it is fast:

- No configure step: the manifest is parsed and planned on every run, in milliseconds.
- Rebuild decisions use content hashes: touching a file, or regenerating one with the same content, rebuilds nothing.
- Header dependencies come from the compiler (`/sourceDependencies`, `-MD`), and the toolchain lookup is cached.
- Up-to-date checks list each folder once instead of asking about every file, and run in parallel.
- Workers hash what a compile read and wrote while other compiles run, so the build's end is not spent hashing.
- One job per core plus two (like Ninja); a job's slot is free the moment its compiler exits; archives and links start as soon as their objects exist.
- Compilers share Anvil's console, even a hidden one, instead of each getting a console of its own.
- Unoptimized MSVC debug builds link incrementally.

## Building Anvil

```sh
cargo build --release              # target/release/anvil(.exe)
cargo test                         # unit tests, then the end-to-end tests
cargo test --test cli workspace    # the end-to-end tests whose name holds `workspace`
```

The end-to-end tests (`tests/cli.rs`) copy a project from `tests/fixtures/` into a temp folder (`anvil-it/`, kept when the test fails) and run the anvil binary in it. A test whose compiler or tool is missing is skipped, saying why; `ANVIL_IT_TOOLCHAINS=gcc,msvc` makes a missing toolchain a failure instead (for CI). `ANVIL_BLESS=1` rewrites `tests/golden/cmake.anvil.toml`, the expected conversion.

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, clippy, the build, the unit tests and the end-to-end tests on Linux (twice: clang, then gcc as the default toolchain), Windows (MSVC and MinGW gcc) and macOS (Apple clang), with `ANVIL_IT_TOOLCHAINS` set so a missing compiler fails.

### Testing Anvil on Linux

```sh
scripts/linux-test.sh                    # everything
scripts/linux-test.sh shared_library     # end-to-end tests whose name holds `shared_library`
scripts/linux-test.sh shell              # a shell in the container, with Anvil built and on PATH
```

`scripts/linux-test.sh` runs the suite in Docker, from Git Bash on Windows, WSL or Linux: the build, the unit tests, and the end-to-end tests twice (clang, then gcc as the default toolchain), both compilers required (`ANVIL_IT_TOOLCHAINS=gcc,clang`). Its arguments go to `cargo test --test cli` (`-- --test-threads=4`). The first run builds the `anvil-linux` image from `scripts/linux/Dockerfile` (`docker rmi anvil-linux` makes the next build it again). The source is copied into a named volume on every run (not `target/` or `out/`: Windows bind mounts are slow), and cargo's target folder and registry stay in named volumes, so reruns rebuild only what changed. To start over: `docker volume ls -q -f name=anvil-linux- | xargs docker volume rm`.
