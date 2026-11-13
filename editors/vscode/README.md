# Anvil for VS Code

Build, run, debug and test C and C++ projects that use Anvil. Open a folder with an
`anvil.toml` and press F5: no `launch.json`, `tasks.json` or other generated file is needed. Everything the extension
knows comes from `anvil targets --json`, so the editor and `anvil build` never disagree.

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
- **Test Explorer:** the tests of `anvil.toml` and every `tests.toml`, at their declarations. Running them calls
  `anvil test` and shows each test's result and output; debugging one builds and debugs its program with the test's
  arguments and folder.
- **anvil.toml and tests.toml:** completion, hover text and unknown-key errors through
  [Even Better TOML](https://marketplace.visualstudio.com/items?itemName=tamasfe.even-better-toml), from the schema
  `anvil schema` prints (kept in the extension's storage and refreshed when anvil changes; no setting to add). Snippets
  for `[target.x]`, `[deps]`, `[[test]]`, `[profile.x]`, `[options]` and build steps. Errors anvil finds in the manifest
  show on the line they are about, as soon as the file is saved.
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
- Manifest completion and validation: Even Better TOML (`tamasfe.even-better-toml`).

## Settings

| Setting | Default | Meaning |
|---|---|---|
| `anvil.path` | `""` | The `anvil` program. Empty: `PATH`, then `~/.cargo/bin`. `~` and `${workspaceFolder}` are expanded. |
| `anvil.configureIntelliSense` | `true` | Point clangd and the C/C++ extension at `compile_commands.json` when their settings are unset. |
| `anvil.debug.console` | `integratedTerminal` | Where the debugged program's console goes (`integratedTerminal`, `internalConsole`, `externalTerminal`). |

## Building the extension

```sh
npm install
npm run compile                 # type check and bundle into dist/
npm test                        # downloads VS Code; runs the suites against a fake anvil
ANVIL_REAL=/path/to/anvil npm test   # also a real project: build, Problems, tests, a debug session to a breakpoint
npm run package                 # anvil-0.1.0.vsix (vsce)
```
