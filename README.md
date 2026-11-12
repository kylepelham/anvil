# Anvil

A fast, zero-ceremony build system for C and C++, written in Rust.

- **No configure step.** `anvil build` reads `anvil.toml`, plans, and builds. There is no build folder to generate, no cache to delete.
- **Small manifests.** Conventions cover the common layout, so a project often needs only a `[project]` table.
- **vcpkg built in.** Anvil finds every vcpkg on the machine, installs what `[deps]` asks for, and wires up includes, libraries and DLLs.
- **Not error prone.** Every table rejects unknown keys (`linkflags` gets "did you mean `link-flags`?"), conditions are checked, a target's settings are its own unless it says `public`, and rebuilds are decided by content hashes and full command lines.
- **CMake conversion.** `anvil convert` runs a CMake project's `CMakeLists.txt` through an interpreter and writes an equivalent `anvil.toml`.

## Quick start

```sh
anvil new hello          # program: src/main.cpp + anvil.toml
cd hello
anvil run                # build and run
anvil build --release
anvil add fmt --to hello # vcpkg port, added to vcpkg.json and [deps]
```

`anvil new mylib --lib` creates a library with a public header in `include/mylib/` and a test in `tests/`.

## Commands

| Command | What it does |
|---|---|
| `anvil build [targets]` (`b`) | Build everything but tests. `--all` adds tests. |
| `anvil run [target] -- args` (`r`) | Build and run a program. |
| `anvil test [filter]` (`t`) | Build and run tests. |
| `anvil clean [--all]` | Delete the profile's output (or everything, vcpkg packages included). |
| `anvil new <name> [--lib]` | Create a project folder. |
| `anvil init` | Create `anvil.toml` for the sources here, or convert the `CMakeLists.txt` here. |
| `anvil add <name>` | Add a dependency: vcpkg by default; `--git`, `--path`, `--pkg-config`, `--system`; `--features`, `--to <target>`. |
| `anvil convert [path]` | Convert a CMake project (`--stdout` to preview, `--force` to overwrite). |
| `anvil convert --check` | Configure the CMake project too and compare both builds' compile commands, file by file. |
| `anvil targets` | List targets. |
| `anvil toolchains` (`doctor`) | Show the compilers Anvil can use. |
| `anvil vcpkg ...` | List, choose or set up vcpkg installations, or run vcpkg itself. |
| `anvil compdb` | Write `compile_commands.json` for clangd. |
| `anvil update [names]` | Move git dependencies to the newest commit of their branch or tag, and vcpkg to its newest ports; rewrite `anvil.lock`. `anvil update json` or `anvil update vcpkg` for one. |

Build options shared by `build`, `run` and `test`:

| Option | Meaning |
|---|---|
| `-r`, `--release` / `-p <name>` | Release profile, or a named profile. |
| `--toolchain <name>` | `msvc`, `clang-cl`, `clang` or `gcc` (default: the project's, else the best installed). |
| `--target <platform>` | Build for another machine: `x86`, `arm64`, `x64`, `arm`, or a triple such as `aarch64-linux-gnu` (see Cross builds). |
| `--locked` | Fail instead of changing `anvil.lock` (for CI). |
| `-o name=true\|false` | Set an option (`-o name`, `-o !name` work too). |
| `-j <n>`, `-k` | Parallel jobs (default: one per core plus two); keep going after a failure. |
| `-v`, `--explain` | Print every command; say why each action runs. |
| `--low-priority` | Run compilers at below-normal priority. |
| `-C <dir>`, `-q` | Run in another folder; print less. |

## Conventions

With no `[target]` tables, a package builds from its layout:

- `src/` with a `main` file is a program; otherwise `src/` is a static library. With no `src/`, the `*.cpp` files beside `anvil.toml` form a program.
- `include/` holds public headers (passed on to dependents), `src/` is a private include folder.
- `tests/`: one test per file, or one test program from all of `tests/` when it has a `main`. Tests link the library.
- Every `[deps]` entry is used by the main target.
- Output goes to `out/<profile>/` (`[project] out` changes the folder).

Explicit targets use the same conventions: their sources default to `src/**/*.{c,cc,cpp,cxx,c++,cu}` under the target's `dir`, `include/` is public and `src/` private. An empty list (`sources = []`, `include = []`, `public.include = []`) turns a convention off.

Files a `when` block lists in `sources` belong to that block: they leave the default sources on every platform and come back only where the block holds. So `when.windows` with `sources = ["src/win/*.cpp"]` keeps `src/**` everywhere, minus `src/win/`, which only Windows compiles. A program or library that ends up with no sources (and no build steps or object libraries to supply them) is an error.

## anvil.toml

```toml
[project]
name = "game"
version = "1.4.0"        # {version}
std = "c++23"            # c++17 | c++20 (default) | c++23 | c++26
c-std = "c17"
toolchain = "msvc"       # default: the best installed
default-run = "game"

[options]
tracy = false
editor = { default = true, description = "Build the editor" }

[deps]
fmt = "vcpkg"
gtest = "vcpkg"
sdl3 = { vcpkg = "sdl3", features = ["vulkan"] }
glm = { git = "https://github.com/g-truc/glm", tag = "1.0.1" }
engine = { path = "../engine" }
zlib = "pkg-config"
user32 = "system"
tracy = { vcpkg = "tracy", if = "tracy" }

[defaults]               # every target
warnings = "high"

[preset.engine-module]   # targets opt in with preset = "engine-module"
werror = true
public.defines = ["ENGINE_MODULE"]

[target.core]
kind = "lib"             # exe | lib | shared | object | headers | test | custom
dir = "engine/core"
preset = "engine-module"
deps = ["sdl3"]          # the target's own
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
libs = ["user32"]

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
deps = ["core"]
gui = true

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
triplet = "x64-windows-static-md"
baseline = "..."
overlay-ports = ["ports"]
```

### Target keys

| Key | Meaning |
|---|---|
| `kind` | `exe`, `lib` (static), `shared`, `object`, `headers`, `test`, `custom`. |
| `dir` | Folder the target's relative paths start from. |
| `if` | The target exists only when this condition holds. |
| `default` | Built by a plain `anvil build` (default true; tests false). |
| `preset` | A `[preset.<name>]` or a list of them, applied before the target's own settings. |
| `sources` | Globs; `!pattern` excludes. C, C++, CUDA (`.cu`), `.rc`, and assembly (`.asm` with MSVC through ml64, `.s`/`.S` with GCC and Clang); headers listed here are ignored. |
| `include`, `system-include` | Include folders, the second with warnings silenced (default: `src/`). |
| `defines`, `flags`, `link-flags` | Definitions; raw compiler and linker settings. `{target:X}` in `link-flags` is X's file, linked first. |
| `deps` | Targets and `[deps]` entries this one uses. |
| `whole-archive` | Static libraries (targets or `[deps]` entries) linked whole, every object in them, also those nothing calls: a program exporting an engine to the plugins it loads. They are linked as deps too. |
| `public` | A table of `include` (default: `include/`), `system-include`, `defines`, `flags` and `deps` for this target and every target using it. A public dep passes its own public settings on. |
| `interface` | A table of `include`, `defines` and `flags` for the targets using this one only (`FOO_API=__declspec(dllimport)`). |
| `libs`, `lib-dirs` | System or prebuilt libraries (`user32`, `pthread`, a `.lib`/`.a` path). |
| `header-deps` | Targets whose include folders and definitions this one uses, without linking them. |
| `needs` | Targets built before this one; nothing of theirs is linked or included (CMake's `add_dependencies`). |
| `export-header` | Write an export macro header: `"mylib_export.h"` or `{ path = "...", macro = "MYLIB_API" }`. It goes under `{gen}`; a shared build defines `<NAME>_SHARED` for everyone and `<NAME>_BUILDING` for the library. |
| `exports` | A program that exports symbols for the modules it loads (an import library on Windows, `-rdynamic` on ELF); modules link against it. |
| `link-depends` | Files whose change relinks the target (a `.def` file, a linker script). |
| `output-name`, `output-prefix`, `output-suffix`, `output-dir` | The output file's name, prefix (`lib`), extension (`.dll`, `.mod`) and folder. |
| `pch`, `unity` | Precompiled header; unity builds. |
| `warnings`, `werror` | `off`, `default`, `high`, `all`; warnings as errors. |
| `arch` | `avx`, `avx2`, `avx512`, `native`. |
| `cuda-arch`, `cuda-flags` | GPUs `.cu` files compile for, as CMake's `CUDA_ARCHITECTURES` reads them (`"native"`, `"all"`, `"86"` for code and PTX, `"86-real"`, `"86-virtual"`, a list or `"75;86"`); extra nvcc options. |
| `std`, `c-std` | Per-target language standard. |
| `gui` | Windows GUI program. |
| `when.<condition>` | A table of the same keys (`kind` too: `when.SHARED_LIBS` with `kind = "shared"`) that applies when the condition holds. |
| `file` | `[[...file]]` rules: `match`, `if`, `defines`, `flags`, `warnings`, `arch`, `opt`. |
| `run`, `pre-build`, `post-build` | Build steps (below). |
| `args`, `workdir`, `timeout`, `env` | For `kind = "test"` (and `[[test]]`). |
| `will-fail`, `after`, `serial` | The test passes by failing; runs after other tests; runs alone. |
| `pass-regex`, `fail-regex`, `skip-exit-code`, `skip-regex` | Judge a test by its output; report it skipped. |
| `expect`, `reject`, `exit-code` | Regexes the output must each match; regexes it must not match; the exit code that passes (default 0). |

Settings are the target's own unless they are under `public` or `interface`; only `[deps]` of a package without `[target]` tables pass on to its users. The old `private-include`, `private-deps`, `public-defines`, `public-flags` and `interface-defines` keys are rejected with the key to write instead.

### Tests

`[[test]]` runs a command (`command`, a list of words or one line split like a shell's; `workdir`, `env`, `timeout`, `if`, `after`, `serial`, `will-fail`, the regexes). Tests go in `anvil.toml` or in a `tests.toml` beside what they test: Anvil reads every `tests.toml` in the package (only for `anvil test` and `anvil targets`), which holds `[[test]]` tables and nothing else, with the same placeholders and paths as `anvil.toml`. A test passes when it exits with `exit-code` (0 unless set), every `expect` regex matches its output (stdout and stderr together) and no `reject` regex does; a failure says which one, and what a `reject` matched. `pass-regex` and `fail-regex` keep CTest's meaning (a matching `pass-regex` passes whatever the exit code). A command that is `cmake -E ...` (`env`, `compare_files`, ...) is run by Anvil itself.

```toml
[[test]]
name = "cpu_check"
command = ["{target:game}", "--headless"]
env = { GAME_TEST_CPU = "fail" }
expect = ["needs a processor with AVX2", "(?s)Missing: .*AVX2"]
reject = ["ERROR"]
exit-code = 1
```

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

An unknown word is an error, not a silent false. The words are reserved: a profile or option named `windows` or `msvc`, or an option named like a profile (or `debug`, `release`), is an error, since the condition could not tell them apart.

### Placeholders

In paths, `defines`, `flags`, `link-flags` and build steps:

| Placeholder | Value |
|---|---|
| `{root}`, `{out}` | The package root; `out/<profile>`. |
| `{gen}`, `{name}` | The target's generated-files folder (on its include path); the target's name. |
| `{profile}`, `{config}` | The profile (`debug`); the same as CMake writes configurations (`Debug`, `RelWithDebInfo`). |
| `{version}` | `[project] version`. |
| `{vcpkg}` | The vcpkg triplet's install folder (`{vcpkg}/include/imgui.h`). |
| `{cxx}`, `{cc}` | The compilers' paths. |
| `{hash:a,b,...}` | 16 hex digits that change when what it lists does, computed at each build: a cache key in a define (`BVH_HASH=0x{hash:compiler,flags,src/bvh.cpp,{root}/external/bvh/*.h}ull`). Items are files and globs (relative to the target's `dir`), `compiler` (the compiler, its version and the machine built for) and `flags` (everything the target's files compile with: definitions, flags, standard, optimization, file rules; not include folders or the hashes themselves). A file that cannot be read or a glob that matches nothing is an error. In target `defines` and `flags`. |

Tests and `[[write]]` take `{root}`, `{out}`, `{profile}`, `{config}`, `{version}` and `{vcpkg}`; tests with a matrix also take their parameters and `{name}`. Build steps also take `{input}`, `{stem}`, `{filename}` (with `for-each`), `{inputs}`, `{output}`, `{outputs}`, `{depfile}`, and `{target:X}` (target X's file; also makes the step wait for X).

### Build steps

`[[target.X.run]]` needs `output` or `outputs`; it re-runs when its command, inputs or outputs change. `for-each` runs the command once per matching file. `inputs` adds files whose change re-runs it; `depfile = true` reads a make-style depfile from `{depfile}`. `shell = true` runs through `cmd /c` or `sh -c`. Generated sources (`.c`, `.cpp`, ...) are compiled into the target.

`[[target.X.pre-build]]` and `[[target.X.post-build]]` take `command`, `shell`, `message`, `workdir` and `if`, and have no outputs. A pre-build step runs before the target compiles, whenever its sources change. A post-build step runs after the file is made, and dependents wait for it.

Steps that run `cmake -E` (`copy`, `copy_if_different`, `copy_directory`, `make_directory`, `remove`, `rm`, `rename`, `touch`, `echo`, `env`, `compare_files`, `cat`, `true`, `false`), as converted CMake projects do, are run by Anvil itself, so no CMake needs to be installed; other `cmake` commands still run cmake.

`[[write]]` writes a file before the build (`path`, `text`, `if`), and rewrites it only when its text changes, so nothing rebuilds for nothing. It is what CMake's configure-time `file(WRITE)` and `configure_file` become.

### Profiles

`debug` (the default) and `release` are built in. `[profile.X]` takes `inherits`, `opt` (`0`–`3`, `"s"`, `"z"`), `debug`, `lto`, `sanitize`, `debug-runtime`, `static-runtime`, `assertions` (false defines `NDEBUG`), `defines`, `flags`, `link-flags`, and `target` (the machine to build for, as `--target` takes it).

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

- MSVC and clang-cl get Visual Studio's environment for that processor (`vcvarsall x64_arm64`); clang gets `--target=<triple>`; gcc uses `<triple>-g++` from `PATH` (or `-m32` for 32-bit on the same system).
- The vcpkg triplet follows (`x86-windows`, `arm64-windows`), and conditions see the target machine: `when.x86`, `when.arm64`, `when.cross`.
- `--target` builds into its own folder (`out/debug-arm64`); a profile's `target` uses the profile's folder.
- Programs that build steps run (`{target:gen}` in a `[[run]]` command) are built a second time for this machine, into `<out>/host`, so code generators work in a cross build.
- `anvil run` and `anvil test` refuse programs this machine cannot start (x64 Windows runs x86 programs; ARM Windows runs x64 ones too).

### Lock file

`anvil.lock`, beside `anvil.toml`, holds the exact versions a build used: each git dependency's commit, the vcpkg ports tree's commit (the `builtin-baseline` that fixes every port's version), and, for review, the port versions installed per triplet. Anvil writes it when it changes and prints what changed; commit it.

- A git dependency with a `branch` or `tag` stays on its locked commit until `anvil update` (or until `anvil.toml` names another branch, tag or `rev`).
- vcpkg stays on its locked baseline even when the vcpkg checkout moves on. With the project's own `vcpkg.json`, its `builtin-baseline` is used instead.
- `--locked` fails the build instead of changing the file, for CI.

### Workspaces

```toml
[workspace]
members = ["engine", "tools/*", "game"]
```

Members are ordinary Anvil packages that depend on each other with `path` deps.

## vcpkg

There is nothing to set up. Anvil looks for vcpkg in `VCPKG_ROOT`, on `PATH`, in Visual Studio installations and in common folders, then:

- installs the ports in `[deps]` (and the project's `vcpkg.json`, if there is one) into `out/vcpkg/installed/<triplet>`, skipping vcpkg entirely when nothing changed;
- with a `vcpkg.json`, turns on the manifest features that list a port `[deps]` uses only there, and accepts a port that a listed one brings in (`vulkan-headers` through `volk`); a port no target on this platform links may be missing;
- picks the triplet from the toolchain and profile (`x64-windows`, `x64-windows-static` with `static-runtime`, `x64-mingw-dynamic` for gcc on Windows), or `[vcpkg] triplet`;
- gives each port's users what its CMake package exports: include subfolders (`include/vma`) and definitions (`FMT_SHARED`), through the packages it links publicly, as CMake would;
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

The choice is stored per user in `%LOCALAPPDATA%/anvil/settings.json`. A project can pin one with `[vcpkg] root = "..."`.

## Converting from CMake

```sh
anvil convert path/to/project --stdout   # preview
anvil convert path/to/project            # write anvil.toml
```

The converter runs the CMake language itself (variables, functions, macros, `if`, `foreach`, `include`, `add_subdirectory`, `list`, `string`, `file(GLOB)`, `cmake_parse_arguments`, ...) and keeps track of which values depend on the platform, compiler and options. Then it:

- writes targets, their includes, defines, flags and dependencies, with `when` blocks for platform- and option-dependent settings; generator expressions become conditions;
- leaves out what the conventions already cover, and turns long file lists back into globs;
- maps compiler flags to Anvil settings (`/W4` and `-Wall -Wextra` to `warnings = "high"`, `/arch:AVX2` and `-mavx2` to `arch = "avx2"`, ...);
- maps `find_package`, `FetchContent` and CPM dependencies to vcpkg ports, git deps or pkg-config;
- converts `add_custom_command` into build steps (`PRE_BUILD`, `PRE_LINK` and `POST_BUILD` into pre- and post-build steps), `add_dependencies` into `needs`, configure-time `file(WRITE/GENERATE/CONFIGURE)` with known text into `[[write]]`;
- converts `add_test`, `gtest_discover_tests` and test properties (`WILL_FAIL`, `DEPENDS`, fixtures, `RESOURCE_LOCK`, `WORKING_DIRECTORY`, regexes, ...) into tests. A test that runs a CMake script whose only work is to run one command and check its exit code and output against regex lists (`cmake -DCOMMAND=... -DEXPECT=... -P run_and_expect.cmake`) becomes a test of that command with `expect`, `reject` and `exit-code`, so it runs without CMake; a script that does anything more (writes files, runs more, reads other variables) stays a `cmake -P` test. Tests go to a `tests.toml` in the folder whose `CMakeLists.txt` adds them (the top folder's stay in `anvil.toml`). Tests of one kind (one helper function, or one program and script) become one `[[test]]` with a `matrix` when that is clearly shorter and every entry fits a line: what the tests share stays in the template, nested `foreach` loops come back as axes, and parameters are named after the loop variables, the helper's arguments or the options they follow (`-DSCRATCH=`, `--view`);
- writes `public` and `interface` sub-tables for `PUBLIC` and `INTERFACE` settings, and `PRIVATE` ones as the target's own;
- turns `file(SHA256 ...)` and `string(SHA256 ...)` of such hashes into `{hash:...}`, so the value follows the files instead of freezing at conversion: a hashed compiler name (`${CMAKE_CXX_COMPILER_ID}`) becomes `compiler`, a hashed `CMakeLists.txt` or `.cmake` file (hashed for the flags it sets) becomes `flags`, and files a `file(GLOB)` found become its pattern; other values only CMake's configure step knows (`file(TIMESTAMP)`, `string(RANDOM)`) are left out of defines and flags with a `# TODO`;
- writes paths without `dir/..` steps (`{root}/cmake/../docs` becomes `{root}/docs`);
- converts `generate_export_header` into `export-header` (or a header written into the project), and an option `BUILD_SHARED_LIBS` into `when.BUILD_SHARED_LIBS` with `kind = "shared"`;
- reads vcpkg settings (triplet, overlay ports and triplets) from `CMakePresets.json`;
- turns `CMAKE_SIZEOF_VOID_P` and `CMAKE_SYSTEM_PROCESSOR` tests into `x64`/`x86`/`arm64` conditions, and CUDA (`LANGUAGES CUDA`, `CUDA_ARCHITECTURES`, `CUDA::` libraries) into `.cu` sources with `cuda-arch`;
- moves settings every target shares into `[defaults]`, settings most targets share into `[preset.common]`, and settings the targets of one helper function share into a preset named after it (`my_add_plugin()` -> `[preset.my-add-plugin]`);
- turns `-Wl,--whole-archive ... -Wl,--no-whole-archive` and `/WHOLEARCHIVE:` into `whole-archive`, and a `for-each` over what a `file(GLOB)` found back into its pattern;
- writes commands as one line when they fit and no word needs quoting, and packs long lists into full lines.

Whatever it cannot convert is marked with a `# TODO` comment or reported as a warning.

### Checking a conversion

```sh
anvil convert --check                        # with the default options
anvil convert --check -o BUILD_TESTS=true    # or others
```

`--check` configures the CMake project with Ninja, the same compiler, options and vcpkg packages (into `out/convert-check`), plans the Anvil build without running it, and compares the two `compile_commands.json` file by file. It lists the files only one side compiles, and for the rest each difference in definitions and include folders, grouped by what differs. It exits with 1 when anything differs. It needs `cmake` and `ninja`.

Limitations: when a variable is set differently in two `if` branches that depend on the platform, only the branch for the converting machine is kept. CMake scripts run by build steps and tests (`cmake -P`), other than the check scripts above, still need cmake on `PATH` (`cmake -E` does not). Values computed at configure time from `execute_process` output cannot be converted; the converter says where they are.

## CUDA

`.cu` files compile with nvcc (from `CUDA_PATH`, `PATH`, or the newest toolkit installed), with the build's runtime library, debug information and definitions; their headers are tracked like any other. The static CUDA runtime links into whatever uses them, and the toolkit's headers and libraries (`libs = ["cublas"]`) are on their users' paths. nvcc accepts only the Visual Studio versions its release knows, so when the build's own is newer, Anvil hands nvcc the newest installed one it accepts (found once and cached). Each `.cu` file compiles whole: separable compilation (device code linked across files) is not supported, and CUDA does not cross-build.

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
cargo build --release    # target/release/anvil(.exe)
cargo test
```
