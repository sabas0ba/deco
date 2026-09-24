'use strict';

/**
 * The `vscode` shim, and in particular the command registry.
 *
 * `createApi` previously had no tests: it only forwards calls to deco, and the
 * Rust tests cover the receiving side. The exception is `$/executeCommand`. It is
 * the only place where the shim *holds state* (the map from command ids to the
 * callbacks an extension registered), and the only call from deco into an
 * extension instead of the reverse. A palette entry that runs an extension
 * command depends on this map.
 */

const test = require('node:test');
const assert = require('node:assert');
const { PassThrough } = require('node:stream');
const { RpcConnection } = require('../src/rpc');
const { createApi } = require('../src/vscode');

/** An api wired to in-memory streams, plus a helper that sends requests as deco. */
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
  // deco may omit `args`. `(...(args ?? []))` turns that into a call with no
  // arguments instead of one `undefined` argument.
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
  // deco matches replies to requests, so a request without a reply would stay
  // in its pending table. A null result avoids that.
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
  // The host still answers afterwards: one failing command must not end the
  // session.
  api.commands.registerCommand('mine.fine', () => 'fine');
  assert.strictEqual((await execute('mine.fine', [])).result, 'fine');
});

test('disposing a command unregisters it', async () => {
  // `context.subscriptions` is disposed on deactivate. A command that remained
  // registered after disposal would be callable after the extension stopped.
  const { api, execute } = connect();
  const registration = api.commands.registerCommand('mine.temporary', () => 'here');
  assert.strictEqual((await execute('mine.temporary', [])).result, 'here');
  registration.dispose();
  const reply = await execute('mine.temporary', []);
  assert.ok(reply.error, 'a disposed command should no longer run');
});

test('registering the same id twice keeps the newer callback', async () => {
  // VS Code rejects this. The shim cannot see other extensions, so within one
  // extension the last registration wins. The catalogue on deco's side prevents
  // two extensions from sharing an id.
  const { api, execute } = connect();
  api.commands.registerCommand('mine.twice', () => 'first');
  api.commands.registerCommand('mine.twice', () => 'second');
  assert.strictEqual((await execute('mine.twice', [])).result, 'second');
});
