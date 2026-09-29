'use strict';

/**
 * The activated extension and the steps that stop it.
 *
 * Stopping follows VS Code's order: the extension's `deactivate` runs first,
 * then the disposables in `context.subscriptions` are disposed, last pushed
 * first. Both `$/deactivate` and `$/shutdown` use this, and it runs at most
 * once, so a `$/shutdown` after a `$/deactivate` does not deactivate again.
 */

/**
 * How long `deactivate` may take, in milliseconds.
 *
 * Shorter than `SHUTDOWN_GRACE` in crates/deco-ext/src/connection.rs (500 ms),
 * after which deco kills the host. The remainder is left for disposing the
 * subscriptions and exiting. When the connection closes without `$/shutdown`,
 * nothing kills the host, and this limit is what stops a `deactivate` that never
 * settles from keeping it running.
 */
const DEACTIVATE_LIMIT_MS = 400;

/**
 * @param {{limitMs?: number}} [options]
 */
function createLifecycle({ limitMs = DEACTIVATE_LIMIT_MS } = {}) {
  let extension = null;
  let context = null;
  let stopping = null;

  /** Resolves with what `deactivate` returned, or rejects at the limit. */
  function bounded(run) {
    let timer;
    const limit = new Promise((_, reject) => {
      timer = setTimeout(
        () => reject(new Error(`deactivate did not finish within ${limitMs} ms`)),
        limitMs,
      );
    });
    return Promise.race([Promise.resolve().then(run), limit]).finally(() => clearTimeout(timer));
  }

  async function stop() {
    const errors = [];
    if (extension && typeof extension.deactivate === 'function') {
      try {
        await bounded(() => extension.deactivate());
      } catch (error) {
        errors.push(error);
      }
    }
    const subscriptions = context ? context.subscriptions.splice(0) : [];
    for (const disposable of subscriptions.reverse()) {
      try {
        if (disposable && typeof disposable.dispose === 'function') {
          disposable.dispose();
        }
      } catch (error) {
        // One disposable that throws must not leave the rest undisposed.
        errors.push(error);
      }
    }
    return errors;
  }

  return {
    /** Records the module and the context passed to its `activate`. */
    activated(module, activationContext) {
      extension = module;
      context = activationContext;
    },

    /**
     * Runs `deactivate` and disposes the subscriptions.
     *
     * Resolves with the errors that were caught, in the order they occurred.
     * Never rejects, because the caller exits afterwards whatever happened.
     * Later calls return the result of the first.
     */
    deactivate() {
      if (!stopping) stopping = stop();
      return stopping;
    },
  };
}

module.exports = { createLifecycle, DEACTIVATE_LIMIT_MS };
