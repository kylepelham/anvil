// The Anvil project open in the workspace: its model from `anvil targets --json` and the selections the status bar shows.

import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { Model, Target, Test, findAnvil, runAnvil } from "./anvil";

const PROFILE_KEY = "anvil.profile";
const TOOLCHAIN_KEY = "anvil.toolchain";
const TARGET_KEY = "anvil.target";
const DEFAULT_PROFILE = "debug";

export const TOOLCHAINS = ["msvc", "clang-cl", "clang", "gcc"];

export class Project implements vscode.Disposable {
  /** The last model anvil printed; kept when a later refresh fails. */
  model: Model | undefined;
  /** Why the last refresh failed, or undefined. */
  error: string | undefined;

  private readonly changeEmitter = new vscode.EventEmitter<void>();
  readonly onDidChange = this.changeEmitter.event;
  private refreshing: Promise<Model | undefined> | undefined;
  private refreshAgain = false;

  constructor(
    readonly context: vscode.ExtensionContext,
    readonly folder: vscode.WorkspaceFolder,
    /** The folder holding the anvil.toml found, where anvil runs. */
    readonly manifestDir: string,
    readonly output: vscode.OutputChannel,
    readonly diagnostics: vscode.DiagnosticCollection,
  ) {}

  dispose(): void {
    this.changeEmitter.dispose();
  }

  get anvil(): string | undefined {
    return findAnvil(this.folder);
  }

  /** The project's root as anvil reports it, else the folder of the anvil.toml found. */
  get root(): string {
    return this.model?.root ?? this.manifestDir;
  }

  /** The profile picked in the status bar, else anvil's default. */
  get profile(): string {
    return this.context.workspaceState.get<string>(PROFILE_KEY) ?? DEFAULT_PROFILE;
  }

  /** The toolchain chosen in the status bar; undefined uses the project's own choice. */
  get toolchainChoice(): string | undefined {
    return this.context.workspaceState.get<string>(TOOLCHAIN_KEY);
  }

  get runnableTargets(): Target[] {
    return this.model?.targets.filter((target) => target.runnable) ?? [];
  }

  /** The target F5 and Run use: the one picked, else the project's default program. */
  get selectedTarget(): Target | undefined {
    const runnable = this.runnableTargets;
    const picked = this.context.workspaceState.get<string>(TARGET_KEY);
    return (
      (picked !== undefined ? findTarget(runnable, picked) : undefined) ??
      (this.model?.default_run ? findTarget(runnable, this.model.default_run) : undefined) ??
      runnable.find((target) => target.default) ??
      runnable[0]
    );
  }

  findTarget(name: string): Target | undefined {
    return this.model ? findTarget(this.model.targets, name) : undefined;
  }

  findTest(name: string): Test | undefined {
    return this.model?.tests.find((test) => test.name === name);
  }

  /** The options that choose what anvil plans: profile and toolchain. */
  planArgs(profile = this.profile, toolchain = this.toolchainChoice): string[] {
    const args = ["-p", profile];
    if (toolchain) {
      args.push("--toolchain", toolchain);
    }
    return args;
  }

  async setProfile(profile: string | undefined): Promise<void> {
    await this.context.workspaceState.update(PROFILE_KEY, profile);
    await this.refresh();
  }

  async setToolchain(toolchain: string | undefined): Promise<void> {
    await this.context.workspaceState.update(TOOLCHAIN_KEY, toolchain);
    await this.refresh();
  }

  async setTarget(target: string | undefined): Promise<void> {
    await this.context.workspaceState.update(TARGET_KEY, target);
    this.changeEmitter.fire();
  }

  /** Read the model again; calls during a refresh coalesce into one more. */
  refresh(): Promise<Model | undefined> {
    if (this.refreshing) {
      this.refreshAgain = true;
      return this.refreshing;
    }
    this.refreshing = (async () => {
      let model: Model | undefined;
      do {
        this.refreshAgain = false;
        model = await this.load();
      } while (this.refreshAgain);
      this.refreshing = undefined;
      this.changeEmitter.fire();
      return model;
    })();
    return this.refreshing;
  }

  /** The model for another profile or toolchain, without changing the selections. */
  async modelFor(profile: string, toolchain: string | undefined): Promise<Model> {
    if (this.model && profile === this.profile && toolchain === this.toolchainChoice) {
      return this.model;
    }
    const anvil = this.requireAnvil();
    const result = await runAnvil(anvil, ["targets", "--json", ...this.planArgs(profile, toolchain)], this.manifestDir);
    if (result.code !== 0) {
      throw new Error(result.stderr.trim() || `anvil targets --json exited with code ${result.code}`);
    }
    return parseModel(result.stdout);
  }

  requireAnvil(): string {
    const anvil = this.anvil;
    if (!anvil) {
      throw new Error("anvil was not found: set `anvil.path`, or put anvil on PATH.");
    }
    return anvil;
  }

  private async load(): Promise<Model | undefined> {
    const anvil = this.anvil;
    if (!anvil) {
      this.error = "anvil was not found: set `anvil.path`, or put anvil on PATH (or in ~/.cargo/bin).";
      this.output.appendLine(this.error);
      return undefined;
    }

    // Planning with a profile the manifest no longer declares would only fail
    const remembered = this.context.workspaceState.get<string>(PROFILE_KEY);
    if (remembered && this.model && !this.model.profiles.includes(remembered)) {
      await this.context.workspaceState.update(PROFILE_KEY, undefined);
    }

    const args = ["targets", "--json", ...this.planArgs()];
    let stderr: string;
    try {
      const result = await runAnvil(anvil, args, this.manifestDir);
      stderr = result.stderr.trim();
      if (result.code === 0) {
        this.model = parseModel(result.stdout);
        this.error = undefined;
        this.diagnostics.clear();
        if (stderr) {
          this.output.appendLine(stderr);
        }
        return this.model;
      }
      if (!stderr) {
        stderr = `anvil ${args.join(" ")} exited with code ${result.code}`;
      }
    } catch (error) {
      stderr = `anvil ${args.join(" ")}: ${error instanceof Error ? error.message : String(error)}`;
    }

    this.error = stderr;
    this.output.appendLine(stderr);
    this.showManifestError(stderr);
    return undefined;
  }

  /** Put anvil's message on the manifest it names, at the line it names (or the table it names) when it can. */
  private showManifestError(message: string): void {
    this.diagnostics.clear();
    const located = /^error: (.+?\.toml): ([\s\S]*)$/.exec(message);
    const file = located ? path.resolve(this.manifestDir, located[1]) : path.join(this.manifestDir, "anvil.toml");
    const text = located ? located[2] : message.replace(/^error: /, "");

    let range = new vscode.Range(0, 0, 0, 0);
    const position = /at line (\d+), column (\d+)/.exec(text);
    if (position) {
      const line = Number(position[1]) - 1;
      const column = Number(position[2]) - 1;
      range = new vscode.Range(line, column, line, column + 1);
    } else {
      const table = /in \[\[?([^\]]+)\]\]?/.exec(text);
      const line = table ? findTableLine(file, table[1]) : undefined;
      if (line !== undefined) {
        range = new vscode.Range(line, 0, line, Number.MAX_SAFE_INTEGER);
      }
    }
    const diagnostic = new vscode.Diagnostic(range, text, vscode.DiagnosticSeverity.Error);
    diagnostic.source = "anvil";
    this.diagnostics.set(vscode.Uri.file(file), [diagnostic]);
  }
}

export function parseModel(text: string): Model {
  const model = JSON.parse(text) as Model;
  if (typeof model !== "object" || model === null || !Array.isArray(model.targets)) {
    throw new Error("anvil targets --json printed something that is not a project model");
  }
  model.profiles ??= [model.profile];
  model.tests ??= [];
  return model;
}

/** Find the anvil.toml a workspace folder holds: its own, else the shallowest one below it. */
export async function findManifestDir(folder: vscode.WorkspaceFolder): Promise<string | undefined> {
  const own = vscode.Uri.joinPath(folder.uri, "anvil.toml");
  try {
    await vscode.workspace.fs.stat(own);
    return folder.uri.fsPath;
  } catch {
    // look deeper
  }
  const found = await vscode.workspace.findFiles(
    new vscode.RelativePattern(folder, "**/anvil.toml"),
    "**/{out,node_modules,.git,vcpkg_installed}/**",
    200,
  );
  const depth = (uri: vscode.Uri) => uri.fsPath.split(/[\\/]/).length;
  found.sort((left, right) => depth(left) - depth(right) || left.fsPath.localeCompare(right.fsPath));
  return found.length > 0 ? path.dirname(found[0].fsPath) : undefined;
}

function findTarget(targets: Target[], name: string): Target | undefined {
  return targets.find((target) => target.qualified === name) ?? targets.find((target) => target.name === name);
}

function findTableLine(file: string, table: string): number | undefined {
  const document = vscode.workspace.textDocuments.find((open) => open.uri.fsPath === file);
  let text: string;
  if (document) {
    text = document.getText();
  } else {
    try {
      text = fs.readFileSync(file, "utf8");
    } catch {
      return undefined;
    }
  }
  const lines = text.split(/\r?\n/);
  const wanted = table.replace(/\s+/g, "");
  const index = lines.findIndex((line) => /^\s*\[/.test(line) && line.replace(/\s+/g, "").replace(/#.*$/, "").replace(/^\[+|\]+$/g, "") === wanted);
  return index >= 0 ? index : undefined;
}
