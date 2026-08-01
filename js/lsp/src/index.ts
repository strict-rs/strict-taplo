import loadTaplo from "../../../crates/taplo-wasm/Cargo.toml";
import {
  asError,
  convertEnv,
  Environment,
  prepareEnv,
} from "@taplo/core";

export interface RpcMessage {
  jsonrpc: "2.0";
  method?: string;
  id?: string | number | null;
  params?: unknown;
  result?: unknown;
  error?: unknown;
}

export interface LspInterface {
  /**
   * Handler for RPC messages set from the LSP server.
   */
  onMessage: (message: RpcMessage) => void;
}

/**
 * Local WebAssembly LSP instance exported by the Rust binding.
 */
interface TaploWasmLspHandle {
  send: (message: RpcMessage) => Promise<void>;
  free: () => void;
}

/**
 * WebAssembly exports consumed by the LSP wrapper.
 */
interface TaploWasmModule {
  initialize: () => void;
  create_lsp: (
    environment: unknown,
    lspInterface: { js_on_message: LspInterface["onMessage"] }
  ) => TaploWasmLspHandle;
}

export class TaploLsp {
  private static modulePromise: Promise<TaploWasmModule> | undefined;

  private constructor(private lspInner: TaploWasmLspHandle) {}

  private static loadModule(): Promise<TaploWasmModule> {
    if (typeof TaploLsp.modulePromise === "undefined") {
      TaploLsp.modulePromise = loadTaplo()
        .then(module => {
          module.initialize();
          return module;
        })
        .catch(error => {
          TaploLsp.modulePromise = undefined;
          throw error;
        });
    }
    return TaploLsp.modulePromise;
  }

  public static async initialize(
    env: Environment,
    lspInterface: LspInterface
  ): Promise<TaploLsp> {
    try {
      const module = await TaploLsp.loadModule();
      prepareEnv(env);

      return new TaploLsp(
        module.create_lsp(convertEnv(env), {
          js_on_message: lspInterface.onMessage,
        })
      );
    } catch (error) {
      throw asError(error);
    }
  }

  public async send(message: RpcMessage): Promise<void> {
    try {
      await this.lspInner.send(message);
    } catch (error) {
      throw asError(error);
    }
  }

  public dispose(): void {
    try {
      this.lspInner.free();
    } catch (error) {
      throw asError(error);
    }
  }
}
