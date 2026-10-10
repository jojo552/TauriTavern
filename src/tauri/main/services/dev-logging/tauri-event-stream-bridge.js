// @ts-check

import { listen as tauriListen, onHostSessionReset } from '../../../../tauri-bridge.js';

/**
 * @template T
 * @param {T} entry
 * @param {Set<(entry: T) => void>} subscribers
 */
function dispatch(entry, subscribers) {
    for (const handler of subscribers) {
        handler(entry);
    }
}

/**
 * Reference-counted owner of one host log stream. The stream is enabled while
 * at least one subscriber is attached and disabled once the last one leaves.
 * The listener is attached before enabling, so the first entries are not lost.
 * Enabling is declarative host state: when a browser page session is replaced,
 * a running stream is enabled again on the new session. Entries produced while
 * disconnected are not replayed; `tail()` / `index()` still hold them.
 *
 * @template T
 * @param {{
 *   safeInvoke: (command: any, args?: any) => Promise<any>;
 *   enableCommand: any;
 *   eventName: string;
 *   listen?: (eventName: string, handler: (event: { payload: T }) => void) => Promise<() => void>;
 *   sessionReset?: (handler: () => Promise<void>) => () => void;
 * }} deps
 */
export function createTauriEventStreamBridge({
    safeInvoke,
    enableCommand,
    eventName,
    listen = tauriListen,
    sessionReset = onHostSessionReset,
}) {
    /** @type {Set<(entry: T) => void>} */
    const subscribers = new Set();
    /** @type {(() => void) | null} */
    let unlisten = null;
    /** @type {Promise<void> | null} */
    let starting = null;
    /** @type {Promise<void> | null} */
    let stopping = null;

    // A start or stop in flight settles against the new session by itself; only a running
    // stream needs to be declared again. A failure surfaces through the reset reporter.
    sessionReset(async () => {
        if (unlisten && !starting && !stopping) {
            await safeInvoke(enableCommand, { enabled: true });
        }
    });

    async function ensureStarted() {
        if (unlisten) {
            return;
        }

        if (starting) {
            await starting;
            return;
        }

        starting = (async () => {
            // A stop for the previous generation may still be in flight; the
            // re-enable must never overtake its disable. A failed disable
            // leaves the host stream running, which re-enabling heals, so it
            // must not block the new start.
            if (stopping) {
                try {
                    await stopping;
                } catch {
                    // See above: the stream is re-enabled right after.
                }
            }
            const stopListening = await listen(eventName, /** @param {{ payload: T }} event */ (event) => {
                dispatch(/** @type {T} */ (event.payload), subscribers);
            });
            try {
                await safeInvoke(enableCommand, { enabled: true });
            } catch (error) {
                stopListening();
                throw error;
            }
            unlisten = stopListening;
        })();

        try {
            await starting;
        } finally {
            starting = null;
        }
    }

    async function stopIfIdle() {
        if (subscribers.size > 0) {
            return;
        }

        if (starting) {
            try {
                await starting;
            } catch {
                // The start failure is reported to its subscriber; there is
                // nothing left running for this stop to tear down.
                return;
            }
            if (subscribers.size > 0) {
                return;
            }
        }

        if (!unlisten) {
            return;
        }

        const stopListening = unlisten;
        unlisten = null;
        stopping = (async () => {
            stopListening();
            await safeInvoke(enableCommand, { enabled: false });
        })();

        try {
            await stopping;
        } finally {
            stopping = null;
        }
    }

    /**
     * @param {(entry: T) => void} handler
     * @returns {Promise<() => Promise<void>>} idempotent disposer resolving once the stop it triggered has settled
     */
    async function subscribe(handler) {
        if (typeof handler !== 'function') {
            throw new Error('handler must be a function');
        }

        subscribers.add(handler);
        try {
            await ensureStarted();
        } catch (error) {
            // A rejected start must not leave a ghost subscriber that a later
            // successful start would dispatch to.
            subscribers.delete(handler);
            throw error;
        }

        let disposed = false;
        return () => {
            if (disposed) {
                return stopping ?? Promise.resolve();
            }
            disposed = true;
            subscribers.delete(handler);
            return stopIfIdle();
        };
    }

    return { subscribe };
}
