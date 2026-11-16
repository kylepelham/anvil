// The Anvil language server, `anvil lsp`: checks, completion, hover, navigation, quick fixes and highlighting of
// anvil.toml and tests.toml all come from it. The extension only starts it and tells it the status bar's selections.

import * as path from "path";
import * as vscode from "vscode";
import {
  CloseAction,
  CloseHandlerResult,
  DidChangeConfigurationNotification,
  ErrorAction,
  ErrorHandlerResult,
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";
import { quoteCommand, shellArgs, spawnOptions } from "./anvil";

// A server that keeps stopping is restarted this many times within the window, then left stopped
const MAX_RESTARTS = 4;
const RESTART_WINDOW_MS = 3 * 60 * 1000;

/** What the server's checks plan with: the profile and toolchain picked in the status bar. */
export interface ServerSettings {
  profile?: string;
  toolchain?: string;
}

export class LanguageServer implements vscode.Disposable {
  /** Why the server is not running (anvil missing, `anvil lsp` failing); undefined while it runs. */
  failure: string | undefined;
  /** The settings the server has, for the tests. */
  sentSettings: ServerSettings | undefined;

  private client: LanguageClient | undefined;
  private anvil: string | undefined;
  private started = false;
  private restarts: number[] = [];
  private queue: Promise<void> = Promise.resolve();
  private readonly output = vscode.window.createOutputChannel("Anvil Language Server");

  constructor(private readonly settings: () => ServerSettings) {}

  dispose(): void {
    void this.stop();
    this.output.dispose();
  }

  get running(): boolean {
    return this.client?.isRunning() ?? false;
  }

  /**
   * Run the server with this anvil (undefined: anvil was not found). The same anvil again does nothing, so a failure
   * is reported once; another anvil, or `force`, starts it again.
   */
  start(anvil: string | undefined, folder: string | undefined, force = false): Promise<void> {
    this.queue = this.queue.then(() => this.startNow(anvil, folder, force)).catch((error) => this.output.appendLine(String(error)));
    return this.queue;
  }

  async stop(): Promise<void> {
    const client = this.client;
    this.client = undefined;
    this.started = false;
    if (client?.needsStop()) {
      await client.stop().catch((error) => this.output.appendLine(`Stopping the server failed: ${error}`));
    }
  }

  /** Tell the server the selections changed; it checks the projects again with them. */
  async updateSettings(): Promise<void> {
    const settings = this.settings();
    if (!this.client || !this.running || JSON.stringify(settings) === JSON.stringify(this.sentSettings)) {
      return;
    }
    this.sentSettings = settings;
    await this.client.sendNotification(DidChangeConfigurationNotification.type, { settings: { anvil: wireSettings(settings) } });
  }

  private async startNow(anvil: string | undefined, folder: string | undefined, force: boolean): Promise<void> {
    if (!force && anvil === this.anvil && (this.client || this.failure)) {
      return;
    }
    await this.stop();
    this.anvil = anvil;
    this.failure = undefined;
    this.restarts = [];
    if (!anvil) {
      this.fail("anvil was not found, so anvil.toml has no checks, completion or hover: set `anvil.path`, or put anvil on PATH.");
      return;
    }

    // Profile and toolchain are the status bar's, not settings: sent at start and on change (updateSettings)
    const cwd = folder ?? path.dirname(anvil);
    const options = spawnOptions(anvil, cwd);
    const serverOptions: ServerOptions = {
      command: quoteCommand(anvil),
      args: shellArgs(anvil, ["lsp"]),
      transport: TransportKind.stdio,
      options: { cwd, env: options.env, shell: options.shell === true },
    };
    const settings = this.settings();
    const clientOptions: LanguageClientOptions = {
      documentSelector: [
        { scheme: "file", pattern: "**/anvil.toml" },
        { scheme: "file", pattern: "**/tests.toml" },
      ],
      initializationOptions: { anvil: wireSettings(settings) },
      outputChannel: this.output,
      initializationFailedHandler: () => false,
      errorHandler: {
        error: (_error, _message, count): ErrorHandlerResult => ({
          action: count !== undefined && count <= 3 ? ErrorAction.Continue : ErrorAction.Shutdown,
          handled: true,
        }),
        closed: () => this.closed(),
      },
    };

    const client = new QuietLanguageClient("anvil", "Anvil", serverOptions, clientOptions);
    this.client = client;
    this.sentSettings = settings;
    try {
      // start() also resolves when the server exits during initialize: only a running client started
      await client.start();
      if (!client.isRunning()) {
        throw new Error("it exited at once");
      }
      this.started = true;
    } catch (error) {
      if (this.client === client) {
        this.client = undefined;
        const reason = error instanceof Error ? error.message : String(error);
        this.fail(`\`anvil lsp\` did not start (${reason}): anvil.toml has no checks, completion or hover. Is ${anvil} an anvil older than \`anvil lsp\`?`);
      }
    }
  }

  /** The server's connection closed: restart a server that ran, a few times; report the rest once. */
  private closed(): CloseHandlerResult {
    if (!this.started) {
      return { action: CloseAction.DoNotRestart, handled: true };
    }

    const now = Date.now();
    this.restarts = this.restarts.filter((time) => now - time < RESTART_WINDOW_MS);
    if (this.restarts.length < MAX_RESTARTS) {
      this.restarts.push(now);
      this.output.appendLine("anvil lsp stopped; starting it again.");
      return { action: CloseAction.Restart, handled: true };
    }

    this.client = undefined;
    this.started = false;
    this.fail(`anvil lsp stopped ${MAX_RESTARTS + 1} times in 3 minutes and was left stopped (Anvil: Refresh starts it again).`);
    return { action: CloseAction.DoNotRestart, handled: true };
  }

  private fail(message: string): void {
    this.failure = message;
    this.output.appendLine(message);
    const show = "Show Output";
    void vscode.window.showErrorMessage(`Anvil: ${message}`, show).then((choice) => {
      if (choice === show) {
        this.output.show(true);
      }
    });
  }
}

/** The settings as sent: an unset one as null, since the server changes only the keys it is given. */
function wireSettings(settings: ServerSettings): { profile: string | null; toolchain: string | null } {
  return { profile: settings.profile ?? null, toolchain: settings.toolchain ?? null };
}

/** A client that logs its errors to the output only: LanguageServer reports failures itself, once. */
class QuietLanguageClient extends LanguageClient {
  override error(message: string, data?: unknown): void {
    super.error(message, data, false);
  }
}
