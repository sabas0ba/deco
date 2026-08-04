'use strict';

const test = require('node:test');
const assert = require('node:assert');
const { install, CapabilityError, normalizeSpecifier } = require('../src/sandbox');

/** Installs the sandbox for the duration of one test. */
function sandboxed(fn) {
  const sandbox = install({ moduleRequire: require, globals: globalThis });
  try {
    fn();
  } finally {
    sandbox.restore();
  }
}

test('node: prefixed specifiers normalise to the bare name', () => {
  assert.strictEqual(normalizeSpecifier('node:fs'), 'fs');
  assert.strictEqual(normalizeSpecifier('fs'), 'fs');
  assert.strictEqual(normalizeSpecifier('lodash'), 'lodash');
});

test('the filesystem module cannot be required', () => {
  sandboxed(() => {
    assert.throws(() => require('fs'), { code: 'DECO_CAPABILITY_DENIED' });
  });
});

test('the node: prefix does not bypass the block', () => {
  sandboxed(() => {
    // Blocking only the bare name would leave an obvious hole.
    assert.throws(() => require('node:fs'), { code: 'DECO_CAPABILITY_DENIED' });
    assert.throws(() => require('node:child_process'), { code: 'DECO_CAPABILITY_DENIED' });
  });
});

test('every network and process module is blocked', () => {
  sandboxed(() => {
    for (const name of ['net', 'http', 'https', 'dgram', 'dns', 'tls', 'child_process', 'worker_threads', 'vm']) {
      assert.throws(() => require(name), { code: 'DECO_CAPABILITY_DENIED' }, name);
    }
  });
});

test('the error names the brokered replacement', () => {
  sandboxed(() => {
    assert.throws(() => require('fs'), (error) => {
      assert.ok(error instanceof CapabilityError);
      assert.match(error.message, /vscode\.workspace\.fs/);
      return true;
    });
    assert.throws(() => require('child_process'), (error) => {
      assert.match(error.message, /vscode\.deco\.spawn/);
      return true;
    });
  });
});

test('harmless modules still load', () => {
  sandboxed(() => {
    assert.ok(require('path').join);
    assert.ok(require('url').URL);
    assert.ok(require('util').format);
  });
});

test('fetch is not reachable', () => {
  sandboxed(() => {
    assert.throws(() => globalThis.fetch, { code: 'DECO_CAPABILITY_DENIED' });
  });
});

test('restore puts the globals back', () => {
  const before = typeof globalThis.fetch;
  const sandbox = install({ moduleRequire: require, globals: globalThis });
  assert.throws(() => globalThis.fetch);
  sandbox.restore();
  assert.strictEqual(typeof globalThis.fetch, before);
  assert.ok(require('fs').existsSync);
});
