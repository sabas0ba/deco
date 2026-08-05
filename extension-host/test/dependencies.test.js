'use strict';

// The extension host has no npm dependencies, and that is a security property
// rather than an accident.
//
// This process is the one part of deco that loads third-party code by design —
// it runs VS Code extensions. Everything it uses to do that (`node:test`,
// `node:worker_threads`, the permission model) ships with Node itself, so the
// only untrusted code in the process is the extension the user chose to
// install. Adding a single npm dependency here would put an unreviewed
// transitive graph *inside* the sandbox host, on the trusted side of the
// boundary the host exists to enforce.
//
// A dependency is a decision, so this test exists to make it a visible one:
// adding to package.json turns CI red, and the person adding it has to say why
// in the same commit that deletes the assertion's expectation.

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
  // Catches the case package.json alone would miss: a `require` that resolves
  // through a globally installed module, or one added to node_modules without
  // being declared. Relative paths and `node:`-prefixed builtins are fine.
  //
  // `node:fs` and `fs` load the same module, but only the second can be
  // shadowed: a package literally named `fs` sitting in node_modules wins the
  // resolution. The prefix is unspoofable, so the host uses it everywhere.
  const srcDir = path.join(__dirname, '..', 'src');
  const offenders = [];

  for (const entry of fs.readdirSync(srcDir)) {
    if (!entry.endsWith('.js')) continue;
    // Comments discuss `require('vscode')` — the specifier extensions use —
    // without performing it, so they are stripped before scanning.
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
