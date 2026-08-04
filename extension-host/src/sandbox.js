'use strict';

/**
 * Removes the ambient authority a Node process normally hands to whatever it
 * loads.
 *
 * This is layer 2 of three (see crates/deco-ext/src/host.rs). Node's own
 * permission model is layer 1 and is the one an extension genuinely cannot talk
 * its way around; it does not cover the network, which is the main gap this
 * file closes. Layer 3 is deco's capability broker, which decides whether a
 * brokered request is actually allowed.
 *
 * The point of this layer is not to be the last line of defence. It is to turn
 * "the extension quietly opened a socket" into "the extension got a clear error
 * telling it to use the deco API", which is both safer and far easier to debug.
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
 * The error an extension sees when it reaches for something it may not have.
 * It names the replacement so the failure is actionable rather than mysterious.
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

/** What to point an extension at when it reaches for a blocked built-in. */
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
 *   which is what extension `require` calls end up in.
 * @param {object} options.globals - The global object to strip.
 * @returns {{restore: () => void}} A handle used only by the test suite; the
 *   real host never restores.
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
      // Defined rather than deleted so that a lazily-installed global (Node
      // installs `fetch` on first access) cannot reappear underneath us.
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
