// IntelliSense: point clangd and the C/C++ extension at the compile_commands.json every anvil build writes.

import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import { Project } from "./project";

const CLANGD = "llvm-vs-code-extensions.vscode-clangd";
const CPPTOOLS = "ms-vscode.cpptools";
const WRITTEN_KEY = "anvil.writtenSettings";

export class IntelliSense {
  /** Whether compile_commands.json existed when last looked at. */
  private databaseExisted: boolean | undefined;

  constructor(private readonly context: vscode.ExtensionContext) {}

  /** Set the workspace settings that are unset (or still hold what Anvil wrote before). */
  async configure(project: Project): Promise<void> {
    const model = project.model;
    if (!model || !vscode.workspace.getConfiguration("anvil", project.folder).get<boolean>("configureIntelliSense", true)) {
      return;
    }
    const database = portable(project.folder, model.compile_commands);
    if (vscode.extensions.getExtension(CLANGD)) {
      await this.setUnlessUserSet(project.folder, "clangd", "arguments", [`--compile-commands-dir=${path.posix.dirname(database)}`]);
    }
    if (vscode.extensions.getExtension(CPPTOOLS)) {
      await this.setUnlessUserSet(project.folder, "C_Cpp", "default.compileCommands", database);
    }
    this.databaseExisted ??= fs.existsSync(model.compile_commands);
  }

  /** After a build: clangd looked for the database at start, so restart it once the first build has written it. */
  async afterBuild(project: Project): Promise<void> {
    const model = project.model;
    if (!model) {
      return;
    }
    const exists = fs.existsSync(model.compile_commands);
    const appeared = exists && this.databaseExisted === false;
    this.databaseExisted = exists;
    if (appeared && vscode.extensions.getExtension(CLANGD)?.isActive) {
      await vscode.commands.executeCommand("clangd.restart");
    }
  }

  private async setUnlessUserSet(folder: vscode.WorkspaceFolder, section: string, key: string, value: unknown): Promise<void> {
    const configuration = vscode.workspace.getConfiguration(section, folder);
    const inspected = configuration.inspect(key);
    const written = this.context.workspaceState.get<Record<string, unknown>>(WRITTEN_KEY, {});
    const id = `${section}.${key}`;
    // In a single-folder workspace the folder's value is the workspace's
    const local = inspected?.workspaceFolderValue ?? inspected?.workspaceValue;
    const userSet = inspected?.globalValue !== undefined || (local !== undefined && JSON.stringify(local) !== JSON.stringify(written[id]));
    if (userSet || JSON.stringify(local) === JSON.stringify(value)) {
      return;
    }
    await configuration.update(key, value, vscode.ConfigurationTarget.Workspace);
    await this.context.workspaceState.update(WRITTEN_KEY, { ...written, [id]: value });
  }
}

/** The path with `${workspaceFolder}` for the folder's part, so the setting holds in every checkout. */
function portable(folder: vscode.WorkspaceFolder, file: string): string {
  const relative = path.relative(folder.uri.fsPath, file);
  const singleFolder = (vscode.workspace.workspaceFolders?.length ?? 0) <= 1;
  if (singleFolder && relative && !relative.startsWith("..") && !path.isAbsolute(relative)) {
    return "${workspaceFolder}/" + relative.split(path.sep).join("/");
  }
  return file.split(path.sep).join("/");
}
