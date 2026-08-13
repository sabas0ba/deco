'use strict';

const test = require('node:test');
const assert = require('node:assert');
const { PassThrough } = require('node:stream');
const { RpcConnection } = require('../src/rpc');

/** A connection wired to in-memory streams, plus the lines it wrote. */
function connect() {
  const input = new PassThrough();
  const output = new PassThrough();
  const written = [];
  output.on('data', (chunk) => {
    for (const line of chunk.toString().split('\n')) {
      if (line.trim()) written.push(JSON.parse(line));
    }
  });
  return { rpc: new RpcConnection(input, output), input, written };
}

/** Lets queued stream events run. */
const tick = () => new Promise((resolve) => setImmediate(resolve));

test('a request is written with an incrementing id', async () => {
  const { rpc, written } = connect();
  rpc.request('fs.readFile', { path: '/a' });
  rpc.request('fs.stat', { path: '/b' });
  await tick();

  assert.deepStrictEqual(written[0], {
    type: 'request',
    id: 1,
    method: 'fs.readFile',
    params: { path: '/a' },
  });
  assert.strictEqual(written[1].id, 2);
});

test('a response resolves the matching request', async () => {
  const { rpc, input } = connect();
  const pending = rpc.request('fs.readFile', { path: '/a' });
  input.write(`${JSON.stringify({ type: 'response', id: 1, result: 'contents' })}\n`);
  assert.strictEqual(await pending, 'contents');
});

test('an error response rejects with the code', async () => {
  const { rpc, input } = connect();
  const pending = rpc.request('fs.readFile', { path: '/etc/passwd' });
  input.write(
    `${JSON.stringify({
      type: 'response',
      id: 1,
      error: { code: 'permissionDenied', message: 'outside every granted scope' },
    })}\n`,
  );
  await assert.rejects(pending, (error) => {
    assert.strictEqual(error.code, 'permissionDenied');
    assert.match(error.message, /outside every granted scope/);
    return true;
  });
});

test('responses resolve out of order', async () => {
  const { rpc, input } = connect();
  const first = rpc.request('a');
  const second = rpc.request('b');
  input.write(`${JSON.stringify({ type: 'response', id: 2, result: 'second' })}\n`);
  input.write(`${JSON.stringify({ type: 'response', id: 1, result: 'first' })}\n`);
  assert.strictEqual(await second, 'second');
  assert.strictEqual(await first, 'first');
});

test('a message split across chunks is reassembled', async () => {
  const { rpc, input } = connect();
  const pending = rpc.request('a');
  const line = JSON.stringify({ type: 'response', id: 1, result: 'ok' });
  input.write(line.slice(0, 10));
  await tick();
  input.write(`${line.slice(10)}\n`);
  assert.strictEqual(await pending, 'ok');
});

test('two messages in one chunk are both delivered', async () => {
  const { rpc, input } = connect();
  const first = rpc.request('a');
  const second = rpc.request('b');
  input.write(
    `${JSON.stringify({ type: 'response', id: 1, result: 1 })}\n` +
      `${JSON.stringify({ type: 'response', id: 2, result: 2 })}\n`,
  );
  assert.strictEqual(await first, 1);
  assert.strictEqual(await second, 2);
});

test('a garbage line does not break the stream', async () => {
  const { rpc, input } = connect();
  const pending = rpc.request('a');
  input.write('this is not json\n');
  input.write('\n');
  input.write(`${JSON.stringify({ type: 'response', id: 1, result: 'survived' })}\n`);
  assert.strictEqual(await pending, 'survived');
});

test('an incoming request reaches its handler and is answered', async () => {
  const { rpc, input, written } = connect();
  rpc.onRequest('$/executeCommand', ({ command }) => `ran ${command}`);
  input.write(
    `${JSON.stringify({ type: 'request', id: 5, method: '$/executeCommand', params: { command: 'x' } })}\n`,
  );
  await tick();
  await tick();
  const reply = written.find((m) => m.type === 'response' && m.id === 5);
  assert.deepStrictEqual(reply, { type: 'response', id: 5, result: 'ran x' });
});

test('a request with no handler is answered with methodNotFound', async () => {
  const { rpc, input, written } = connect();
  input.write(`${JSON.stringify({ type: 'request', id: 6, method: 'nope', params: {} })}\n`);
  await tick();
  const reply = written.find((m) => m.type === 'response' && m.id === 6);
  assert.strictEqual(reply.error.code, 'methodNotFound');
});

test('a throwing handler becomes an error response rather than a crash', async () => {
  const { rpc, input, written } = connect();
  rpc.onRequest('boom', () => {
    throw new Error('exploded');
  });
  input.write(`${JSON.stringify({ type: 'request', id: 7, method: 'boom', params: {} })}\n`);
  await tick();
  await tick();
  const reply = written.find((m) => m.type === 'response' && m.id === 7);
  assert.strictEqual(reply.error.code, 'operationFailed');
  assert.match(reply.error.message, /exploded/);
});

test('a response for an unknown id is ignored', async () => {
  const { rpc, input } = connect();
  input.write(`${JSON.stringify({ type: 'response', id: 999, result: 'stray' })}\n`);
  await tick();
  assert.strictEqual(rpc.pendingCount, 0);
});

test('notifications reach their handler and expect no reply', async () => {
  const { rpc, input, written } = connect();
  let seen = null;
  rpc.onNotification('$/shutdown', (params) => {
    seen = params;
  });
  input.write(`${JSON.stringify({ type: 'notification', method: '$/shutdown', params: { a: 1 } })}\n`);
  await tick();
  assert.deepStrictEqual(seen, { a: 1 });
  assert.strictEqual(written.length, 0);
});

test('an unknown message type is ignored', async () => {
  const { rpc, input, written } = connect();
  input.write(`${JSON.stringify({ type: 'somethingElse', id: 1 })}\n`);
  await tick();
  assert.strictEqual(written.length, 0);
  assert.strictEqual(rpc.pendingCount, 0);
});

test('deco closing the connection is reported exactly once', async () => {
  // The host exits on this, so it has to fire — a host that outlives deco keeps
  // whatever it was granted, and in a container keeps the container too. Once,
  // because `end` and `close` both arrive and exiting twice is a race.
  const { rpc, input } = connect();
  let closed = 0;
  rpc.onClosed(() => {
    closed += 1;
  });
  input.end();
  await tick();
  await tick();
  assert.strictEqual(closed, 1);
});

test('a connection deco never closes reports nothing', async () => {
  const { rpc, input } = connect();
  let closed = 0;
  rpc.onClosed(() => {
    closed += 1;
  });
  input.write(`${JSON.stringify({ type: 'notification', method: '$/nothing' })}\n`);
  await tick();
  assert.strictEqual(closed, 0);
});
