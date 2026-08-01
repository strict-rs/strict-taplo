import type { Readable, Writable } from "node:stream";

/**
 * Environment required for several Taplo functions.
 *
 * This is required because WebAssembly is not self-contained and is sand-boxed.
 */
export interface Environment {
  /**
   * Return the current date.
   */
  now: () => Date;
  /**
   * Return the environment variable, if any.
   */
  envVar: (name: string) => string | undefined;
  /**
   * Return all environment variables as `[key, value]` pairs.
   */
  envVars: () => Array<[string, string]>;
  /**
   * Return whether the standard error output is a tty or not.
   */
  stdErrAtty: () => boolean;
  /**
   * Read `n` bytes from the standard input.
   *
   * If the returned array is empty, EOF is reached.
   *
   * This function must not return more than `n` bytes.
   */
  stdin: Readable | ((n: number) => Promise<Uint8Array>);
  /**
   * Write the given bytes to the standard output returning
   * the number of bytes written.
   */
  stdout: Writable | ((bytes: Uint8Array) => Promise<number>);
  /**
   * Write the given bytes to the standard error output returning
   * the number of bytes written.
   */
  stderr: Writable | ((bytes: Uint8Array) => Promise<number>);
  /**
   * Search a glob file pattern and return the matched files.
   */
  glob: (pattern: string) => Array<string>;
  /**
   * Read the contents of the file at the given path.
   */
  readFile: (path: string) => Promise<Uint8Array>;
  /**
   * Atomically replace or create a file at the given path.
   *
   * Resolve the promise only after the complete byte sequence is visible at the
   * destination. Implementations should use a same-directory temporary file
   * followed by an atomic replacement, or the host platform's equivalent.
   */
  writeFile: (path: string, bytes: Uint8Array) => Promise<void>;
  /**
   * Turn an URL into a file path.
   */
  urlToFilePath: (url: string) => string | undefined;
  /**
   * Turn a file path into an absolute file URL.
   */
  filePathToUrl: (path: string) => string | undefined;
  /**
   * Return whether a path is absolute.
   */
  isAbsolute: (path: string) => boolean;
  /**
   * Return the path to the current working directory.
   */
  cwd: () => string | undefined;
  /**
   * Find the Taplo config file from the given directory
   * and return the path if found.
   *
   * The following files should be searched in order from the given root:
   *
   * - `.taplo.toml`
   * - `taplo.toml`
   */
  findConfigFile: (from: string) => string | undefined;
  /**
   * The fetch function if it is not defined on the global Window.
   *
   * This is required for environments like NodeJs where the fetch API is not available,
   * so a package like `node-fetch` must be used instead.
   *
   */
  fetch?: {
    fetch: any;
    Headers: any;
    Request: any;
    Response: any;
  };
}

/**
 * Failure while preparing browser-compatible HTTP globals for the WebAssembly transport.
 */
export class EnvironmentSetupError extends Error {
  public constructor(message: string) {
    super(message);
    this.name = "EnvironmentSetupError";
  }
}

/**
 * Preserve JavaScript and WebAssembly errors while normalizing non-error throws.
 */
export function asError(thrown: unknown): Error {
  if (thrown instanceof Error) {
    return thrown;
  }
  return new Error(String(thrown));
}

/**
 * @private
 */
export function prepareEnv(environment: Environment): void {
  if (typeof globalThis.fetch === "function") {
    return;
  }

  const bindings = environment.fetch;
  if (typeof bindings?.fetch !== "function") {
    throw new EnvironmentSetupError(
      "fetch is unavailable; provide complete HTTP bindings in Environment.fetch"
    );
  }

  const globals: Array<[string, unknown]> = [
    ["Headers", bindings.Headers],
    ["Request", bindings.Request],
    ["Response", bindings.Response],
    ["fetch", bindings.fetch],
  ];
  for (const [name, binding] of globals) {
    if (typeof binding !== "function") {
      throw new EnvironmentSetupError(
        `Environment.fetch.${name} must be callable`
      );
    }
  }
  for (const [name, binding] of globals) {
    if (!Reflect.set(globalThis, name, binding)) {
      throw new EnvironmentSetupError(
        `failed to install Environment.fetch.${name}`
      );
    }
  }
}

/**
 * @private
 */
export function convertEnv(env: Environment) {
  const stdin =
    typeof env.stdin === "function" ? env.stdin : streamToReadCb(env.stdin);
  const stdout =
    typeof env.stdout === "function" ? env.stdout : streamToWriteCb(env.stdout);
  const stderr =
    typeof env.stderr === "function" ? env.stderr : streamToWriteCb(env.stderr);

  return {
    js_now: () => env.now(),
    js_env_var: (name: string) => env.envVar(name),
    js_env_vars: () => env.envVars(),
    js_atty_stderr: () => env.stdErrAtty(),
    js_on_stdin: stdin,
    js_on_stdout: stdout,
    js_on_stderr: stderr,
    js_glob_files: (pattern: string) => env.glob(pattern),
    js_read_file: (path: string) => env.readFile(path),
    js_write_file: (path: string, bytes: Uint8Array) =>
      env.writeFile(path, bytes),
    js_to_file_path: (url: string) => env.urlToFilePath(url),
    js_to_file_url: (path: string) => env.filePathToUrl(path),
    js_is_absolute: (path: string) => env.isAbsolute(path),
    js_cwd: () => env.cwd(),
    js_find_config_file: (from: string) => env.findConfigFile(from),
  };
}

function streamToWriteCb(
  stream: Writable
): (bytes: Uint8Array) => Promise<number> {
  return bytes => {
    return new Promise((resolve, reject) => {
      stream.write(bytes, error => {
        if (error) {
          reject(error);
          return;
        }
        resolve(bytes.length);
      });
    });
  };
}

function streamToReadCb(stream: Readable): (n: number) => Promise<Uint8Array> {
  // The stream EOF event callback is immediately called after the last
  // bit of data was read, however we cannot immediately signal it as we are still returning data.
  //
  // If EOF happens, subsequent stream events will not happen, not even "end" and the promise
  // will get stuck and nodejs will terminate without any errors (found it out the hard way).
  //
  // So we keep track of EOF here and immediately return 0 bytes on the next call without
  // touching the stream.
  let eof = false;

  return n => {
    return new Promise((resolve, reject) => {
      if (eof || stream.readableEnded) {
        eof = true;
        return resolve(new Uint8Array());
      }

      let settled = false;

      function cleanup() {
        stream.off("readable", onReadable);
        stream.off("end", onEnd);
        stream.off("error", onError);
      }

      function settleData(data: unknown) {
        if (settled) {
          return;
        }
        settled = true;
        cleanup();
        if (data instanceof Uint8Array) {
          resolve(data);
        } else {
          reject(new TypeError("stdin stream returned a non-byte chunk"));
        }
      }

      function onReadable() {
        const data = stream.read(n);
        if (data !== null) {
          settleData(data);
        }
      }

      function onEnd() {
        eof = true;
        if (!settled) {
          settled = true;
          cleanup();
          resolve(new Uint8Array());
        }
      }

      function onError(error: Error) {
        if (!settled) {
          settled = true;
          cleanup();
          reject(error);
        }
      }

      const immediate = stream.read(n);
      if (immediate !== null) {
        settleData(immediate);
        return;
      }

      stream.on("readable", onReadable);
      stream.once("end", onEnd);
      stream.once("error", onError);
    });
  };
}
