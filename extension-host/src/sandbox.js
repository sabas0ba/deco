'use strict';

/**
 * Removes the access to built-in modules and network globals that a Node process
 * normally gives to any code it loads.
 *
 * This is layer 2 of the four layers numbered 0 to 3 in
 * crates/deco-ext/src/host.rs. Layer 0 is the optional container around the
 * Node process. Layer 1 is Node's permission model, which an extension cannot
 * bypass from JavaScript. It does not cover the network, and this file mainly
 * fills that gap. Layer 3 is deco's capability broker, which decides whether a
 * brokered request is allowed.
 *
 * This layer is not the last line of defence. Its purpose is that an extension
 * trying to open a socket gets a clear error telling it to use the deco API,
 * instead of succeeding unnoticed. This is safer and easier to debug.
 */

/** Built-ins an extension must never reach directly. */
const BLOCKED_MODULES = new Set([
  'child_process',
  'cluster',
  'dgram',
  'dns',
  'fs',
  'fs/promises',
  'http',
  'http2',
  'https',
  'inspector',
  'net',
  'os',
  'process',
  'repl',
  'tls',
  'v8',
  'vm',
  'worker_threads',
]);

/** Globals that reach the network without going through `require`. */
const BLOCKED_GLOBALS = [
  'fetch',
  'WebSocket',
  'XMLHttpRequest',
  'EventSource',
  'navigator',
];

/**
 * The error an extension gets when it accesses a blocked module or global.
 * It names the replacement API so the user knows what to use instead.
 */
class CapabilityError extends Error {
  constructor(what, replacement) {
    super(
      `deco: '${what}' is not available to extensions. ` +
        (replacement
          ? `Use ${replacement} instead; deco will check the capability your ` +
            'manifest declared and ask the user if needed.'
          : 'This capability has no brokered equivalent.'),
    );
    this.name = 'CapabilityError';
    this.code = 'DECO_CAPABILITY_DENIED';
  }
}

/** The replacement API to suggest for each blocked built-in. */
const REPLACEMENTS = {
  fs: 'vscode.workspace.fs',
  'fs/promises': 'vscode.workspace.fs',
  http: 'vscode.deco.fetch',
  https: 'vscode.deco.fetch',
  net: 'vscode.deco.fetch',
  child_process: 'vscode.deco.spawn',
  process: 'vscode.env',
  os: 'vscode.env',
};

/**
 * Normalises a specifier so `node:fs` and `fs` are treated identically.
 */
function normalizeSpecifier(specifier) {
  return specifier.startsWith('node:') ? specifier.slice(5) : specifier;
}

/**
 * Installs the sandbox. Call once, before any extension code is loaded.
 *
 * @param {object} options
 * @param {NodeRequire} options.moduleRequire - The `Module` class's require,
 *   which extension `require` calls go through.
 * @param {object} options.globals - The global object to strip.
 * @returns {{restore: () => void}} A handle that undoes the changes. The test
 *   suite uses it between tests; the host calls it just before exiting, on
 *   `$/shutdown` and when the connection to deco closes.
 */
function install({ moduleRequire, globals }) {
  const Module = moduleRequire('module');
  const originalLoad = Module._load;
  const removedGlobals = new Map();

  Module._load = function decoGuardedLoad(specifier, parent, isMain) {
    const name = normalizeSpecifier(specifier);
    if (BLOCKED_MODULES.has(name)) {
      throw new CapabilityError(specifier, REPLACEMENTS[name]);
    }
    return originalLoad.call(this, specifier, parent, isMain);
  };

  for (const name of BLOCKED_GLOBALS) {
    if (name in globals) {
      removedGlobals.set(name, globals[name]);
      // Redefined instead of deleted, so a lazily installed global cannot
      // reappear later (Node installs `fetch` on first access).
      Object.defineProperty(globals, name, {
        configurable: true,
        get() {
          throw new CapabilityError(name, 'vscode.deco.fetch');
        },
      });
    }
  }

  return {
    restore() {
      Module._load = originalLoad;
      for (const [name, value] of removedGlobals) {
        Object.defineProperty(globals, name, {
          configurable: true,
          writable: true,
          enumerable: false,
          value,
        });
      }
      removedGlobals.clear();
    },
  };
}

module.exports = {
  install,
  CapabilityError,
  BLOCKED_MODULES,
  BLOCKED_GLOBALS,
  normalizeSpecifier,
};
