'use strict';

// The extension host has no npm dependencies. This is an intentional security
// property.
//
// This process is the only part of deco that loads third-party code by design:
// it runs VS Code extensions. Everything it uses for that (`node:test`,
// `node:worker_threads`, the permission model) ships with Node, so the only
// untrusted code in the process is the extension the user installed. A single
// npm dependency would put an unreviewed transitive dependency graph *inside*
// the sandbox host, on the trusted side of the boundary the host enforces.
//
// This test makes adding a dependency an explicit decision. Adding one to
// package.json fails CI, and the commit that changes this assertion must
// explain why.

const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');

const manifest = JSON.parse(
  fs.readFileSync(path.join(__dirname, '..', 'package.json'), 'utf8'),
);

test('the host declares no runtime or build dependencies', () => {
  for (const field of [
    'dependencies',
    'devDependencies',
    'peerDependencies',
    'optionalDependencies',
    'bundledDependencies',
  ]) {
    const declared = Object.keys(manifest[field] ?? {});
    assert.deepStrictEqual(
      declared,
      [],
      `package.json grew a "${field}" entry: ${declared.join(', ')}. See the ` +
        'comment at the top of this file before removing this assertion.',
    );
  }
});

test('nothing under src requires a package outside the standard library', () => {
  // Catches cases that package.json alone would miss: a `require` that resolves
  // to a globally installed module, or to one added to node_modules without
  // being declared. Relative paths and `node:`-prefixed builtins are allowed.
  //
  // `node:fs` and `fs` load the same module, but only `fs` can be shadowed: a
  // package named `fs` in node_modules takes precedence in resolution. The
  // prefix cannot be shadowed, so the host uses it everywhere.
  const srcDir = path.join(__dirname, '..', 'src');
  const offenders = [];

  for (const entry of fs.readdirSync(srcDir)) {
    if (!entry.endsWith('.js')) continue;
    // Comments mention `require('vscode')`, the specifier extensions use, without
    // calling it, so comments are stripped before scanning.
    const source = fs
      .readFileSync(path.join(srcDir, entry), 'utf8')
      .replace(/\/\*[\s\S]*?\*\//g, '')
      .replace(/(^|[^:])\/\/.*$/gm, '$1');
    for (const match of source.matchAll(/require\(\s*'([^']+)'\s*\)/g)) {
      const specifier = match[1];
      const isRelative = specifier.startsWith('.');
      const isBuiltin = specifier.startsWith('node:');
      if (!isRelative && !isBuiltin) {
        offenders.push(`${entry}: require('${specifier}')`);
      }
    }
  }

  assert.deepStrictEqual(
    offenders,
    [],
    `bare require() of a non-builtin:\n  ${offenders.join('\n  ')}\n` +
      "Use the 'node:' prefix for standard-library modules; anything else is " +
      'a new dependency.',
  );
});
