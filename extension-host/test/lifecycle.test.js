'use strict';

const test = require('node:test');
const assert = require('node:assert');
const { createLifecycle } = require('../src/lifecycle');

/** A context whose subscriptions record their disposal in `order`. */
function contextRecording(order, names) {
  return {
    subscriptions: names.map((name) => ({
      dispose() {
        order.push(name);
      },
    })),
  };
}

test('deactivate runs before the subscriptions, which are disposed last first', async () => {
  const order = [];
  const lifecycle = createLifecycle();
  lifecycle.activated(
    {
      async deactivate() {
        await new Promise((resolve) => setTimeout(resolve, 5));
        order.push('deactivate');
      },
    },
    contextRecording(order, ['first', 'second', 'third']),
  );

  assert.deepStrictEqual(await lifecycle.deactivate(), []);
  assert.deepStrictEqual(order, ['deactivate', 'third', 'second', 'first']);
});

test('a module without deactivate still has its subscriptions disposed', async () => {
  const order = [];
  const lifecycle = createLifecycle();
  lifecycle.activated({}, contextRecording(order, ['only']));

  assert.deepStrictEqual(await lifecycle.deactivate(), []);
  assert.deepStrictEqual(order, ['only']);
});

test('nothing activated is not an error', async () => {
  assert.deepStrictEqual(await createLifecycle().deactivate(), []);
});

test('a failing deactivate and a failing disposable do not stop the rest', async () => {
  const order = [];
  const context = contextRecording(order, ['first', 'last']);
  context.subscriptions.splice(1, 0, {
    dispose() {
      throw new Error('dispose failed');
    },
  });
  const lifecycle = createLifecycle();
  lifecycle.activated(
    {
      deactivate() {
        throw new Error('deactivate failed');
      },
    },
    context,
  );

  const errors = await lifecycle.deactivate();
  assert.deepStrictEqual(
    errors.map((error) => error.message),
    ['deactivate failed', 'dispose failed'],
  );
  assert.deepStrictEqual(order, ['last', 'first']);
});

test('a deactivate that never settles is abandoned at the limit', async () => {
  const order = [];
  const lifecycle = createLifecycle({ limitMs: 20 });
  lifecycle.activated(
    { deactivate: () => new Promise(() => {}) },
    contextRecording(order, ['only']),
  );

  const errors = await lifecycle.deactivate();
  assert.strictEqual(errors.length, 1);
  assert.match(errors[0].message, /within 20 ms/);
  // The subscriptions are disposed even though `deactivate` did not finish.
  assert.deepStrictEqual(order, ['only']);
});

test('deactivating twice runs deactivate once', async () => {
  let calls = 0;
  const order = [];
  const lifecycle = createLifecycle();
  lifecycle.activated(
    {
      deactivate() {
        calls += 1;
      },
    },
    contextRecording(order, ['only']),
  );

  const first = lifecycle.deactivate();
  const second = lifecycle.deactivate();
  await Promise.all([first, second]);
  await lifecycle.deactivate();
  assert.strictEqual(calls, 1);
  assert.deepStrictEqual(order, ['only']);
});
