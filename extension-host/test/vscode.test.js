'use strict';

/**
 * The `vscode` shim, and in particular the command registry.
 *
 * `createApi` had no tests: everything it does is forward a call to deco, and the
 * Rust side's tests covered the receiving end. The exception is
 * `$/executeCommand`, which is the one place the shim *holds state* — the map of
 * command ids to the callbacks an extension registered — and the one call deco
 * makes into an extension rather than the other way round. A palette entry that
 * runs an extension command depends entirely on this map.
 */

const test = require('node:test');
const assert = require('node:assert');
const { PassThrough } = require('node:stream');
const { RpcConnection } = require('../src/rpc');
const { createApi } = require('../src/vscode');

/** An api wired to in-memory streams, plus a way to speak to it as deco. */
function connect() {
  const input = new PassThrough();
  const output = new PassThrough();
  const written = [];
  output.on('data', (chunk) => {
    for (const line of chunk.toString().split('\n')) {
      if (line.trim()) written.push(JSON.parse(line));
    }
  });
  const rpc = new RpcConnection(input, output);
  const api = createApi(rpc, { extensionId: 'test.extension' });
  let id = 100;
  /** Sends `$/executeCommand` as deco would, and resolves with the reply. */
  const execute = async (command, args) => {
    const mine = (id += 1);
    input.write(`${JSON.stringify({ type: 'request', id: mine, method: '$/executeCommand', params: { command, args } })}\n`);
    for (let i = 0; i < 50; i += 1) {
      await new Promise((resolve) => setImmediate(resolve));
      const reply = written.find((m) => m.type === 'response' && m.id === mine);
      if (reply) return reply;
    }
    throw new Error(`no reply to ${command}`);
  };
  return { api, execute, written };
}

test('a registered command runs and its return value goes back to deco', async () => {
  const { api, execute } = connect();
  api.commands.registerCommand('mine.hello', () => 'hello from the host');
  const reply = await execute('mine.hello', []);
  assert.strictEqual(reply.result, 'hello from the host');
  assert.strictEqual(reply.error, undefined);
});

test('registering tells deco the name, which is how it reaches a palette', async () => {
  const { api, written } = connect();
  api.commands.registerCommand('mine.hello', () => 0);
  await new Promise((resolve) => setImmediate(resolve));
  const announced = written.find((m) => m.method === 'commands.registerCommand');
  assert.ok(announced, `nothing announced it: ${JSON.stringify(written)}`);
  assert.deepStrictEqual(announced.params, { command: 'mine.hello' });
});

test('arguments are passed through in order', async () => {
  const { api, execute } = connect();
  api.commands.registerCommand('mine.add', (a, b) => a + b);
  const reply = await execute('mine.add', [2, 40]);
  assert.strictEqual(reply.result, 42);
});

test('a command with no arguments is called with none rather than with undefined', async () => {
  // deco may send `args` absent entirely; `(...(args ?? []))` is what makes that
  // an empty call instead of one argument that happens to be undefined.
  const { api, execute } = connect();
  api.commands.registerCommand('mine.count', (...given) => given.length);
  assert.strictEqual((await execute('mine.count', undefined)).result, 0);
  assert.strictEqual((await execute('mine.count', [])).result, 0);
});

test('an async command is awaited', async () => {
  const { api, execute } = connect();
  api.commands.registerCommand('mine.later', async () => {
    await new Promise((resolve) => setImmediate(resolve));
    return 'eventually';
  });
  assert.strictEqual((await execute('mine.later', [])).result, 'eventually');
});

test('a command that returns nothing answers null rather than dropping the reply', async () => {
  // deco correlates replies to requests, so a request that never gets one is a
  // leak in its pending table — worse than a null.
  const { api, execute } = connect();
  api.commands.registerCommand('mine.quiet', () => {});
  const reply = await execute('mine.quiet', []);
  assert.strictEqual(reply.result, null);
  assert.strictEqual(reply.error, undefined);
});

test('an unregistered command is an error naming it, not a dropped connection', async () => {
  const { execute } = connect();
  const reply = await execute('mine.absent', []);
  assert.ok(reply.error, `expected an error: ${JSON.stringify(reply)}`);
  assert.match(reply.error.message, /mine\.absent/);
});

test('a command that throws reports the reason and the host stays up', async () => {
  const { api, execute } = connect();
  api.commands.registerCommand('mine.explode', () => {
    throw new Error('it went wrong');
  });
  const reply = await execute('mine.explode', []);
  assert.match(reply.error.message, /it went wrong/);
  // Still answering afterwards: one failing command must not end the session.
  api.commands.registerCommand('mine.fine', () => 'fine');
  assert.strictEqual((await execute('mine.fine', [])).result, 'fine');
});

test('disposing a command unregisters it', async () => {
  // `context.subscriptions` disposes on deactivate, so a command that outlived
  // its disposal would be callable after the extension stopped.
  const { api, execute } = connect();
  const registration = api.commands.registerCommand('mine.temporary', () => 'here');
  assert.strictEqual((await execute('mine.temporary', [])).result, 'here');
  registration.dispose();
  const reply = await execute('mine.temporary', []);
  assert.ok(reply.error, 'a disposed command should no longer run');
});

test('registering the same id twice keeps the newer callback', async () => {
  // VS Code refuses this; the shim cannot see the other extensions to refuse it
  // the same way, so the last writer wins *within one extension* and the
  // catalogue on deco's side is what stops two extensions sharing an id.
  const { api, execute } = connect();
  api.commands.registerCommand('mine.twice', () => 'first');
  api.commands.registerCommand('mine.twice', () => 'second');
  assert.strictEqual((await execute('mine.twice', [])).result, 'second');
});
