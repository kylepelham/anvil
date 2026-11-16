# Anvil for VS Code

Build, run, debug and test C and C++ projects that use Anvil. Open a folder with an
`anvil.toml` and press F5: no `launch.json`, `tasks.json` or other generated file is needed. The extension holds no
build logic of its own:

- the project (targets, programs, tests, toolchain) comes from `anvil targets --json`, so the editor and `anvil build`
  never disagree;
- everything about editing `anvil.toml` and `tests.toml` comes from Anvil's language server, `anvil lsp`, which the
  extension starts (the same server any LSP editor can use).

## Features

- **F5 debugs the selected program.** It runs `anvil build <target> -p <profile>` in a terminal, then starts the
  C/C++ extension's debugger: `cppvsdbg` for MSVC and clang-cl builds, `cppdbg` with gdb or lldb otherwise. The program
  gets the arguments, folder and environment `anvil run` would give it. A failed build stops there, with its errors in
  Problems. Ctrl+F5 runs the program in a terminal (`anvil run`). Both apply while the workspace has no `launch.json`
  configuration selected; with one, F5 does what it says.
- **Status bar:** the profile (`debug`, `release`, every `[profile.X]`), the toolchain and the target to run; click one to
  change it. The choice is remembered per workspace.
- **Tasks** (type `anvil`): `anvil: build` is the default build task (Ctrl+Shift+B) and `anvil: test` the default test
  task, plus `build <target>`, `run <target>` and `clean`. Compiler errors and warnings go to Problems and link to the
  file and line (cl, clang-cl, gcc and clang).
- **Run and Debug** lists a configuration per program and per test that runs one. In `launch.json`:

  ```jsonc
  { "type": "anvil", "request": "launch", "name": "Game", "target": "game" }
  { "type": "anvil", "request": "launch", "name": "Smoke test", "test": "smoke" }
  ```

  Optional keys: `profile`, `toolchain`, `args` (in place of `anvil run`'s), `cwd`, `env` (added), `build` (false skips
  the build), `stopAtEntry`; any other key (`visualizerFile`, `setupCommands`, `console`, ...) goes to the C/C++
  debugger as is.
- **Test Explorer:** the tests of `anvil.toml` and every `tests.toml`, at their declarations (from `anvil lsp`'s
  document symbols). Running them calls `anvil test` and shows each test's result and output; debugging one builds and
  debugs its program with the test's arguments and folder.
- **anvil.toml and tests.toml** (from `anvil lsp`): the errors `anvil build` would report, on the word they are about,
  as you type (the real loader runs on the unsaved text); completion of keys, values, target and dependency names,
  condition words, `{placeholders}` and vcpkg ports; snippets for `[target.x]`, `[deps]`, `[[test]]`, `[profile.x]`,
  `[options]` and build steps; hover text; go to definition, find references and rename of targets, dependencies,
  profiles and options; outline; did-you-mean quick fixes; colouring of what Anvil gives a meaning (condition words,
  placeholders, target names). The status bar's profile and toolchain are the server's too. A manifest error also turns
  the status bar's profile item red (click: open anvil.toml); it is listed in Problems once, by the server.
- **Syntax highlighting:** the extension's own TOML grammar for `anvil.toml` and `tests.toml` (language *Anvil TOML*),
  so no TOML extension is needed and none checks these files a second time.
- **IntelliSense:** clangd (`clangd.arguments`) and the C/C++ extension (`C_Cpp.default.compileCommands`) are pointed at
  the `compile_commands.json` every build writes, in the workspace settings, unless you set them yourself.
- **Commands** (Anvil: ...): Build, Build Selected Target, Run, Test, Clean, Debug Target, Select Profile, Select
  Target, Select Toolchain, Refresh, Open anvil.toml, Convert CMake Project (runs `anvil convert`).

Editing `anvil.toml` or a `tests.toml` re-reads the project.

## Requirements

- `anvil` on `PATH`, in `~/.cargo/bin`, or named by `anvil.path`.
- Debugging: the [C/C++ extension](https://marketplace.visualstudio.com/items?itemName=ms-vscode.cpptools)
  (`ms-vscode.cpptools`), which provides the debuggers. With gcc or clang, the gdb or lldb Anvil finds for the
  toolchain.
- An anvil that has `anvil lsp` for the anvil.toml features. Without one, the rest works and a message says so once.

## Settings

| Setting | Default | Meaning |
|---|---|---|
| `anvil.path` | `""` | The `anvil` program. Empty: `PATH`, then `~/.cargo/bin`. `~` and `${workspaceFolder}` are expanded. |
| `anvil.configureIntelliSense` | `true` | Point clangd and the C/C++ extension at `compile_commands.json` when their settings are unset. |
| `anvil.debug.console` | `integratedTerminal` | Where the debugged program's console goes (`integratedTerminal`, `internalConsole`, `externalTerminal`). |
| `anvil.trace.server` | `off` | `messages` or `verbose`: log the language server's traffic and work to the *Anvil Language Server* output. |

Changing `anvil.path` restarts the language server; so does Anvil: Refresh (after rebuilding anvil in place, say).

A TOML extension such as Even Better TOML does not apply to `anvil.toml` and `tests.toml` (they are *Anvil TOML*). To
have its formatter there anyway, map them back with `"files.associations": { "anvil.toml": "toml" }`; the language
server still runs on them (it goes by file name), but then both check the TOML syntax.

## How it fits together

| Part | Comes from |
|---|---|
| Problems, completion, hover, navigation, quick fixes and semantic colours in anvil.toml / tests.toml | `anvil lsp` (vscode-languageclient, stdio) |
| Syntax colours of anvil.toml / tests.toml | `syntaxes/anvil-toml.tmLanguage.json` (plain TOML) |
| Status bar, tasks, F5 / Run and Debug, Test Explorer, IntelliSense settings | `anvil targets --json`, `anvil build`, `anvil run`, `anvil test` (the tests' places: `anvil lsp`) |
| Compiler errors in Problems | the tasks' problem matchers |

## Building the extension

```sh
npm ci
npm run compile                 # type check and bundle into dist/
cargo build                     # (in the repository) the anvil whose `anvil lsp` the lsp suite runs
npm test                        # downloads VS Code; runs the suites against a fake anvil, and the lsp suite against
                                # ../../target/debug/anvil (or ANVIL_LSP)
ANVIL_REAL=/path/to/anvil npm test   # also a real project: build, Problems, tests, a debug session to a breakpoint
npm run package                 # anvil-0.1.0.vsix (vsce)
```
