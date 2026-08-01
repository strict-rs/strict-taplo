import {
  Config,
  asError,
  convertEnv,
  Environment,
  FormatterOptions,
  prepareEnv,
} from "@taplo/core";
import loadTaplo from "../../../crates/taplo-wasm/Cargo.toml";
import { objectCamel } from "./util";

/**
 * Options for the format function.
 */
export interface FormatOptions {
  /**
   * Options to pass to the formatter.
   */
  options?: FormatterOptions;

  /**
   * Taplo configuration, this can be parsed
   * from files like `taplo.toml` or provided manually.
   */
  config?: Config;
}

/**
 * Options for TOML Lint.
 */
export interface LintOptions {
  /**
   * Taplo configuration, this can be parsed
   * from `.taplo.toml` or provided manually.
   */
  config?: Config;
}
/**
 * An lint error.
 */
export interface LintError {
  /**
   * A range within the TOML document if any.
   */
  range?: Range;
  /**
   * The error message.
   */
  error: string;
}

/**
 * The object returned from the lint function.
 */
export interface LintResult {
  /**
   * Lint errors, if any.
   *
   * This includes syntax, semantic and schema errors as well.
   */
  errors: Array<LintError>;
}

/**
 * WebAssembly exports consumed by the synchronous library wrapper.
 */
interface TaploWasmModule {
  initialize: () => void;
  lint: (
    environment: unknown,
    toml: string,
    config: unknown
  ) => Promise<LintResult>;
  format: (
    environment: unknown,
    toml: string,
    options: unknown,
    config: unknown
  ) => string;
  from_json: (json: string) => string;
  to_json: (toml: string) => string;
}

/**
 * This class allows for usage of the library in a synchronous context
 * after being asynchronously initialized once.
 *
 * It cannot be constructed with `new`, and instead must be
 * created by calling `initialize`.
 *
 * Example usage:
 *
 * ```js
 * import { Taplo } from "taplo";
 *
 * // Somewhere at the start of your app.
 * const taplo = await Taplo.initialize();
 * // ...
 * // The other methods will not return promises.
 * const formatted = taplo.format(tomlDocument);
 * ```
 */
export class Taplo {
  private static modulePromise: Promise<TaploWasmModule> | undefined;

  private constructor(
    private env: Environment,
    private wasm: TaploWasmModule
  ) {}

  private static loadModule(): Promise<TaploWasmModule> {
    if (typeof Taplo.modulePromise === "undefined") {
      Taplo.modulePromise = loadTaplo()
        .then(module => {
          module.initialize();
          return module;
        })
        .catch(error => {
          Taplo.modulePromise = undefined;
          throw error;
        });
    }
    return Taplo.modulePromise;
  }

  public static async initialize(env?: Environment): Promise<Taplo> {
    try {
      const module = await Taplo.loadModule();
      const environment = env ?? browserEnvironment();
      prepareEnv(environment);
      return new Taplo(environment, module);
    } catch (error) {
      throw asError(error);
    }
  }

  /**
   * Lint a TOML document, this function returns
   * both syntax and semantic (e.g. conflicting keys) errors.
   *
   * If a JSON schema is found in the config, the TOML document will be validated with it
   * only if it is syntactically valid.
   *
   * Example usage:
   *
   * ```js
   * const lintResult = await taplo.lint(tomlDocument, {
   *   config: { schema: { url: "https://example.com/my-schema.json" } },
   * });
   *
   * if (lintResult.errors.length > 0) {
   *   throw new Error("the document is invalid");
   * }
   * ```
   *
   * @param toml TOML document.
   * @param options Optional additional options.
   */
  public async lint(toml: string, options?: LintOptions): Promise<LintResult> {
    try {
      return await this.wasm.lint(
        convertEnv(this.env),
        toml,
        objectCamel(options?.config ?? {})
      );
    } catch (error) {
      throw asError(error);
    }
  }

  /**
   * Format the given TOML document.
   *
   * @param toml TOML document.
   * @param options Optional format options.
   */
  public format(toml: string, options?: FormatOptions): string {
    try {
      return this.wasm.format(
        convertEnv(this.env),
        toml,
        options?.options ?? {},
        objectCamel(options?.config ?? {})
      );
    } catch (error) {
      throw asError(error);
    }
  }

  /**
   * Encode the given JavaScript object to TOML.
   *
   * @throws If the given object cannot be serialized to TOML.
   *
   * @param data JSON compatible JavaScript object or JSON string.
   */
  public encode(data: object | string): string {
    if (typeof data !== "string") {
      data = JSON.stringify(data);
    }

    try {
      return this.wasm.from_json(data);
    } catch (error) {
      throw asError(error);
    }
  }

  /**
   * Decode the given TOML string to a JavaScript object.
   *
   * @throws If data is not valid TOML.
   *
   * @param {string} data TOML string.
   */
  public decode<T extends object = any>(data: string): T;

  /**
   * Convert the given TOML string to JSON.
   *
   * @throws If data is not valid TOML.
   *
   * @param data TOML string.
   * @param {boolean} parse Whether to keep the JSON in a string format.
   */
  public decode(data: string, parse: false): string;

  public decode<T extends object = any>(
    data: string,
    parse: boolean = true
  ): T | string {
    let v: string;
    try {
      v = this.wasm.to_json(data);
    } catch (error) {
      throw asError(error);
    }

    if (parse) {
      return JSON.parse(v);
    } else {
      return v;
    }
  }
}

/**
 * A very limited default environment inside a browser.
 */
function browserEnvironment(): Environment {
  return {
    cwd: () => "/",
    envVar: () => undefined,
    envVars: () => [],
    findConfigFile: () => undefined,
    glob: () => [],
    isAbsolute: () => true,
    now: () => new Date(),
    readFile: () => Promise.reject(new Error("file reads are unavailable")),
    writeFile: () => Promise.reject(new Error("file writes are unavailable")),
    stderr: async bytes => {
      console.error(new TextDecoder().decode(bytes));
      return bytes.length;
    },
    stdErrAtty: () => false,
    stdin: () => Promise.reject(new Error("standard input is unavailable")),
    stdout: async bytes => {
      console.log(new TextDecoder().decode(bytes));
      return bytes.length;
    },
    filePathToUrl: filePath => new URL(filePath, "file:///").href,
    urlToFilePath: (url: string) => url.slice("file://".length),
  };
}
