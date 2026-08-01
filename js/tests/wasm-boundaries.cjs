"use strict";

const assert = require("node:assert/strict");
const path = require("node:path");
const { Readable, Writable } = require("node:stream");

const { convertEnv } = require("../core/dist/index.js");
const { Taplo } = require("../lib/dist/index.js");
const { TaploLsp } = require("../lsp/dist/index.js");

function environment(overrides = {}) {
  return {
    cwd: () => "/workspace",
    envVar: () => undefined,
    envVars: () => [],
    findConfigFile: () => undefined,
    glob: () => [],
    isAbsolute: value => path.posix.isAbsolute(value),
    now: () => new Date("2026-01-01T00:00:00.000Z"),
    readFile: async () => {
      throw new Error("file reads are unavailable");
    },
    writeFile: async () => {
      throw new Error("file writes are unavailable");
    },
    stderr: async bytes => bytes.length,
    filePathToUrl: value => new URL(value, "file:///").href,
    stdErrAtty: () => false,
    stdin: async () => new Uint8Array(),
    stdout: async bytes => bytes.length,
    urlToFilePath: url => new URL(url).pathname,
    ...overrides,
  };
}

async function expectRejected(label, operation, fragments) {
  let rejected;
  try {
    await operation();
  } catch (error) {
    rejected = error;
  }

  assert.ok(rejected instanceof Error, `${label} must reject with an Error`);
  for (const fragment of fragments) {
    assert.ok(
      rejected.message.includes(fragment),
      `${label} must mention ${JSON.stringify(fragment)}; received ${JSON.stringify(rejected.message)}`
    );
  }
}

async function withDeadline(label, operation) {
  let timeout;
  try {
    return await Promise.race([
      operation(),
      new Promise((_, reject) => {
        timeout = setTimeout(
          () => reject(new Error(`${label} did not settle`)),
          2_000
        );
      }),
    ]);
  } finally {
    clearTimeout(timeout);
  }
}

function expectThrown(label, operation, fragments) {
  let thrown;
  try {
    operation();
  } catch (error) {
    thrown = error;
  }

  assert.ok(thrown instanceof Error, `${label} must throw an Error`);
  for (const fragment of fragments) {
    assert.ok(
      thrown.message.includes(fragment),
      `${label} must mention ${JSON.stringify(fragment)}; received ${JSON.stringify(thrown.message)}`
    );
  }
}

async function main() {
  const input = Readable.from([Buffer.from("abc")]);
  const output = [];
  const convertedStreams = convertEnv(
    environment({
      stdin: input,
      stdout: new Writable({
        write(bytes, _encoding, callback) {
          output.push(Buffer.from(bytes));
          callback();
        },
      }),
    })
  );
  assert.equal(
    Buffer.from(
      await withDeadline(
        "the first stream read",
        () => convertedStreams.js_on_stdin(3)
      )
    ).toString("utf8"),
    "abc",
    "the stream adapter must return the bytes read from Node input"
  );
  assert.equal(
    (
      await withDeadline(
        "the post-data EOF read",
        () => convertedStreams.js_on_stdin(3)
      )
    ).length,
    0,
    "the stream adapter must report EOF after returning the final data chunk"
  );
  assert.equal(
    await convertedStreams.js_on_stdout(Uint8Array.from([120, 121])),
    2,
    "the stream adapter must report the exact successful write length"
  );
  assert.equal(
    Buffer.concat(output).toString("utf8"),
    "xy",
    "the stream adapter must forward the exact output bytes"
  );

  const wrongChunkStreams = convertEnv(
    environment({
      stdin: Readable.from([{ not: "bytes" }], { objectMode: true }),
    })
  );
  await expectRejected(
    "wrong Node input chunk type",
    () => wrongChunkStreams.js_on_stdin(1),
    ["non-byte chunk"]
  );

  const failedOutput = new Writable({
    write(_bytes, _encoding, callback) {
      callback(new Error("stream write rejected"));
    },
  });
  failedOutput.on("error", () => {});
  const failedWriterStreams = convertEnv(
    environment({ stdout: failedOutput })
  );
  await expectRejected(
    "rejected Node output stream",
    () => failedWriterStreams.js_on_stdout(Uint8Array.from([1])),
    ["stream write rejected"]
  );

  const taplo = await Taplo.initialize(environment());

  assert.equal(
    taplo.format("value=1\n"),
    "value = 1\n",
    "formatting must preserve the successful JavaScript wire contract"
  );
  assert.equal(
    taplo.format("value=1\n", {
      config: { schema: { path: "schemas/project.json" } },
    }),
    "value = 1\n",
    "schema paths must prepare through the host path-to-file-URL capability"
  );
  assert.deepEqual(
    await taplo.lint("value = 1\n"),
    { errors: [] },
    "linting without an associated schema must return an empty diagnostic result"
  );
  expectThrown(
    "TOML-to-JSON syntax diagnostics",
    () => taplo.decode("value = [1 2]\n"),
    ["TOML input", "syntax diagnostic"]
  );
  expectThrown(
    "TOML-to-JSON semantic diagnostics",
    () => taplo.decode("value = 1\nvalue = 2\n"),
    ["TOML input", "semantic diagnostic"]
  );

  const wrongTypeTaplo = await Taplo.initialize(
    environment({ isAbsolute: () => "yes" })
  );
  expectThrown(
    "wrong synchronous callback return type",
    () =>
      wrongTypeTaplo.format("value = 1\n", {
        config: { include: ["relative.toml"] },
      }),
    ["js_is_absolute", "boolean"]
  );

  const throwingTaplo = await Taplo.initialize(
    environment({
      isAbsolute: () => {
        throw new Error("absolute-path callback exploded");
      },
    })
  );
  expectThrown(
    "thrown synchronous callback",
    () =>
      throwingTaplo.format("value = 1\n", {
        config: { include: ["relative.toml"] },
      }),
    ["js_is_absolute", "absolute-path callback exploded"]
  );

  const malformedFileUrlTaplo = await Taplo.initialize(
    environment({ filePathToUrl: () => "not a URL" })
  );
  expectThrown(
    "malformed path-to-file-URL callback result",
    () =>
      malformedFileUrlTaplo.format("value = 1\n", {
        config: { schema: { path: "schema.json" } },
      }),
    ["js_to_file_url", "invalid URL", "not a URL"]
  );

  const wrongFileUrlTaplo = await Taplo.initialize(
    environment({ filePathToUrl: () => 42 })
  );
  expectThrown(
    "wrong path-to-file-URL callback return type",
    () =>
      wrongFileUrlTaplo.format("value = 1\n", {
        config: { schema: { path: "schema.json" } },
      }),
    ["js_to_file_url", "string", "number"]
  );

  const rejectedReadTaplo = await Taplo.initialize(
    environment({
      readFile: async () => {
        throw new Error("schema read rejected");
      },
    })
  );
  await expectRejected(
    "rejected asynchronous callback",
    () =>
      rejectedReadTaplo.lint("value = 1\n", {
        config: { schema: { url: "file:///workspace/schema.json" } },
      }),
    ["js_read_file", "schema read rejected"]
  );

  const wrongReadTaplo = await Taplo.initialize(
    environment({
      readFile: async () => "not bytes",
    })
  );
  await expectRejected(
    "wrong asynchronous callback return type",
    () =>
      wrongReadTaplo.lint("value = 1\n", {
        config: { schema: { url: "file:///workspace/schema.json" } },
      }),
    ["js_read_file", "Uint8Array"]
  );

  await expectRejected(
    "missing LSP output callback",
    () => TaploLsp.initialize(environment(), {}),
    ["js_on_message", "missing"]
  );
  await expectRejected(
    "thrown clock callback",
    () =>
      TaploLsp.initialize(
        environment({
          now: () => {
            throw new Error("clock exploded");
          },
        }),
        { onMessage: () => {} }
      ),
    ["js_now", "clock exploded"]
  );
  await expectRejected(
    "wrong clock return type",
    () =>
      TaploLsp.initialize(
        environment({ now: () => ({}) }),
        { onMessage: () => {} }
      ),
    ["js_now", "Date or RFC 3339 string"]
  );
  await expectRejected(
    "malformed timestamp",
    () =>
      TaploLsp.initialize(
        environment({ now: () => "not-a-timestamp" }),
        { onMessage: () => {} }
      ),
    ["js_now", "invalid timestamp"]
  );

  const messages = [];
  const lsp = await TaploLsp.initialize(environment(), {
    onMessage: message => messages.push(message),
  });
  await lsp.send({
    jsonrpc: "2.0",
    id: 1,
    method: "initialize",
    params: {
      processId: null,
      clientInfo: { name: "strict-taplo-wasm-boundary-test" },
      locale: "en",
      rootPath: null,
      rootUri: null,
      initializationOptions: {},
      capabilities: {},
      trace: "off",
      workspaceFolders: [],
    },
  });
  assert.ok(
    messages.some(message => message.id === 1 && typeof message.result === "object"),
    "successful local LSP construction must emit the initialize response"
  );
  await lsp.send({
    jsonrpc: "2.0",
    id: 2,
    method: "shutdown",
  });
  await lsp.send({
    jsonrpc: "2.0",
    method: "exit",
  });
  lsp.dispose();
}

main().catch(error => {
  process.stderr.write(`${error instanceof Error ? error.stack : String(error)}\n`);
  process.exitCode = 1;
});
