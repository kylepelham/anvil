// Finding and running the anvil program, and the shape of `anvil targets --json`.

import * as childProcess from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";

export interface Toolchain {
  family: string;
  abi: string;
  platform: string;
  debugger: string;
  debugger_path: string | null;
}

export interface Target {
  name: string;
  package: string;
  qualified: string;
  kind: string;
  default: boolean;
  runnable: boolean;
  output: string | null;
  workdir: string;
  args: string[];
  env: Record<string, string>;
  dir: string;
  gui: boolean;
}

export interface Test {
  name: string;
  program: string | null;
  command: string[];
  workdir: string;
  env?: Record<string, string>;
  /** What `anvil test -L` picks it by (absent from versions of Anvil before labels). */
  labels?: string[];
}

/** What `anvil targets --json` prints (version 1). */
export interface Model {
  version: number;
  root: string;
  profile: string;
  profiles: string[];
  toolchain: Toolchain;
  out_dir: string;
  compile_commands: string;
  default_run: string | null;
  targets: Target[];
  tests: Test[];
}

export interface RunResult {
  code: number;
  stdout: string;
  stderr: string;
}

/** Where the anvil program is: the `anvil.path` setting, else PATH, else ~/.cargo/bin. */
export function findAnvil(folder?: vscode.WorkspaceFolder): string | undefined {
  const setting = vscode.workspace.getConfiguration("anvil", folder).get<string>("path", "").trim();
  if (setting) {
    return expandPath(setting, folder);
  }

  // PATH, as a shell would search it
  const names = process.platform === "win32" ? ["anvil.exe", "anvil.cmd", "anvil.bat"] : ["anvil"];
  for (const directory of (process.env.PATH ?? "").split(path.delimiter)) {
    if (!directory) {
      continue;
    }
    for (const name of names) {
      const candidate = path.join(directory.replace(/^"|"$/g, ""), name);
      if (isFile(candidate)) {
        return candidate;
      }
    }
  }

  // cargo install puts it here, and it is not always on PATH for GUI programs
  const cargoHome = process.env.CARGO_HOME ?? path.join(os.homedir(), ".cargo");
  const cargo = path.join(cargoHome, "bin", process.platform === "win32" ? "anvil.exe" : "anvil");
  return isFile(cargo) ? cargo : undefined;
}

/** Run anvil to completion and collect its output. */
export function runAnvil(anvil: string, args: string[], cwd: string, token?: vscode.CancellationToken): Promise<RunResult> {
  return new Promise((resolve, reject) => {
    const child = childProcess.spawn(quoteCommand(anvil), shellArgs(anvil, args), spawnOptions(anvil, cwd));
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8").on("data", (chunk: string) => (stdout += chunk));
    child.stderr.setEncoding("utf8").on("data", (chunk: string) => (stderr += chunk));
    child.on("error", reject);
    child.on("close", (code) => resolve({ code: code ?? 1, stdout, stderr }));
    token?.onCancellationRequested(() => killTree(child));
  });
}

/** Options for spawning anvil; a .cmd or .bat wrapper needs a shell on Windows. */
export function spawnOptions(anvil: string, cwd: string): childProcess.SpawnOptionsWithoutStdio {
  const env: NodeJS.ProcessEnv = { ...process.env, NO_COLOR: "1" };
  delete env.CLICOLOR_FORCE;
  return { cwd, env, shell: isBatch(anvil), windowsHide: true };
}

/** Quote an argument for cmd.exe when anvil is a batch file (spawn with shell: true joins the words as they are). */
export function shellArgs(anvil: string, args: string[]): string[] {
  if (!isBatch(anvil)) {
    return args;
  }
  return args.map((arg) => (/^[\w./:=\\-]+$/.test(arg) ? arg : `"${arg.replace(/"/g, '""')}"`));
}

export function quoteCommand(anvil: string): string {
  return isBatch(anvil) && anvil.includes(" ") ? `"${anvil}"` : anvil;
}

/** Stop a process and everything it started. */
export function killTree(child: childProcess.ChildProcess): void {
  if (child.pid === undefined || child.exitCode !== null) {
    return;
  }
  if (process.platform === "win32") {
    childProcess.spawn("taskkill", ["/PID", String(child.pid), "/T", "/F"], { windowsHide: true });
  } else {
    child.kill("SIGTERM");
  }
}

export function isFile(file: string): boolean {
  try {
    return fs.statSync(file).isFile();
  } catch {
    return false;
  }
}

/** The same path, compared case-insensitively on Windows and with either slash. */
export function samePath(left: string, right: string): boolean {
  const normalize = (text: string) => {
    const resolved = path.resolve(text);
    return process.platform === "win32" ? resolved.toLowerCase() : resolved;
  };
  return normalize(left) === normalize(right);
}

function isBatch(anvil: string): boolean {
  return process.platform === "win32" && /\.(cmd|bat)$/i.test(anvil);
}

function expandPath(text: string, folder?: vscode.WorkspaceFolder): string {
  const workspaceFolder = folder ?? vscode.workspace.workspaceFolders?.[0];
  let expanded = text.replace(/^~(?=$|[\\/])/, os.homedir());
  if (workspaceFolder) {
    expanded = expanded.replace(/\$\{workspaceFolder\}/g, workspaceFolder.uri.fsPath);
  }
  return path.resolve(workspaceFolder?.uri.fsPath ?? process.cwd(), expanded);
}
