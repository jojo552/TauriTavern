// Explicit browser transport for the Rust HTTP host. This is not a Tauri runtime shim.
import { createWebResources } from './resources.js';

// Avoid compression overhead for small frames. Only the byte owner marks text as compressible;
// opaque staging uploads and extension-store blobs retain their raw representation.
const GZIP_MIN_BYTES = 32 * 1024;
const CONNECTION_TIMEOUT_MS = 90_000;

// Only transport loss, never a domain, authentication or malformed-protocol response.
export class WebConnectionError extends Error {
    constructor(message, options) {
        super(message, options);
        this.name = 'WebConnectionError';
    }
}
export function isWebConnectionError(error) { return error instanceof WebConnectionError; }

export class WebSessionResetError extends Error {
    constructor() {
        super('Server page session was reset; pending command outcomes may be unknown. Old files and cold swipe sources must be reopened.');
        this.name = 'WebSessionResetError';
    }
}
export function isWebSessionResetError(error) { return error instanceof WebSessionResetError; }

export class WebAuthenticationError extends Error {
    constructor() {
        super('登录状态已失效，请通过登录页重新登录。当前操作不会自动重发。');
        this.name = 'WebAuthenticationError';
    }
}

async function gzip(bytes) {
    const stream = new Blob([bytes]).stream().pipeThrough(new CompressionStream('gzip'));
    return new Uint8Array(await new Response(stream).arrayBuffer());
}

export function createWebTransport({
    fetch = globalThis.fetch?.bind(globalThis),
    EventSource = globalThis.EventSource,
    onError = error => globalThis.reportError ? globalThis.reportError(error) : console.error(error),
    schedule = globalThis.setTimeout,
    unschedule = globalThis.clearTimeout,
    now = Date.now,
    window = globalThis.window,
    document = globalThis.document,
} = {}) {
    if (typeof fetch !== 'function' || typeof EventSource !== 'function') throw new Error('Web transport requires fetch and EventSource');
    let session, source, starting, failure, connectionTimer, timerToken, initialReady, sessionRequest;
    let lastActivity = 0;
    let epoch = 0, nextChannelId = 0, nextListenerId = 0, lastEventId = -1, retryDelay = 1000;
    let connectionState;
    const resources = createWebResources();
    const channels = new Map(), listeners = new Map();
    const resetHandlers = new Set(), requests = new Set(), connectedWaiters = new Set();
    const stopped = Promise.withResolvers();
    stopped.promise.catch(() => {});

    function state(value) {
        connectionState = value;
    }

    function clearConnectionTimer() {
        unschedule(connectionTimer);
        connectionTimer = timerToken = undefined;
    }

    // One timer alternates between connection liveness and reconnect backoff. A queued callback
    // from a cancelled timer cannot open a second stream after a wake/reset/dispose.
    function timeConnection(callback, delay) {
        clearConnectionTimer();
        const token = timerToken = {};
        connectionTimer = schedule(() => {
            if (timerToken !== token || failure) return;
            connectionTimer = timerToken = undefined;
            callback();
        }, delay);
    }

    function watchConnection(events, generation) {
        timeConnection(() => {
            if (events !== source || generation !== epoch) return;
            const remaining = CONNECTION_TIMEOUT_MS - (now() - lastActivity);
            if (remaining > 0) watchConnection(events, generation);
            else reconnect();
        }, Math.max(0, CONNECTION_TIMEOUT_MS - (now() - lastActivity)));
    }

    function reconnect(immediate = false, brokenSource = false) {
        if (failure || !starting || sessionRequest) return;
        // Repeated failure/wake signals join the existing attempt instead of closing it again.
        if (!brokenSource && connectionState === 'reconnecting' && source && now() - lastActivity < CONNECTION_TIMEOUT_MS) return;
        if (source) {
            source.close();
            source = undefined;
            clearConnectionTimer();
        }
        state('reconnecting');
        if (timerToken && !immediate) return;
        const generation = epoch;
        timeConnection(() => { void openConnection(generation, initialReady); }, immediate ? 0 : retryDelay);
        if (!immediate) retryDelay = Math.min(10_000, retryDelay * 2);
        // HTTP outcomes are independent; neither cancel nor replay any invoke here.
    }

    function wake() {
        if (failure || !starting) return;
        if (!source || now() - lastActivity >= CONNECTION_TIMEOUT_MS) reconnect(true);
    }
    const visibilityChanged = () => { if (document.visibilityState === 'visible') wake(); };
    const pageShown = event => { if (event.persisted) wake(); };
    const pageHidden = event => {
        if (!event.persisted) void dispose().catch(onError);
    };
    document?.addEventListener('visibilitychange', visibilityChanged);
    window?.addEventListener('online', wake);
    window?.addEventListener('pageshow', pageShown);
    window?.addEventListener('pagehide', pageHidden);

    function removeLifecycleHooks() {
        document?.removeEventListener('visibilitychange', visibilityChanged);
        window?.removeEventListener('online', wake);
        window?.removeEventListener('pageshow', pageShown);
        window?.removeEventListener('pagehide', pageHidden);
    }

    function rejectConnectedWaiters(error) {
        for (const waiter of connectedWaiters) waiter.reject(error);
    }

    function rejectRequests(error) {
        for (const request of requests) {
            request.controller.abort(error);
            request.reject(error);
        }
    }

    // Ends this page's transport locally without reporting a failure. Used by page exit and by an
    // explicit sign-out; both are intended endings, not errors to recover from or surface.
    function shutdown(reason) {
        if (failure) return false;
        source?.close();
        source = undefined;
        clearConnectionTimer();
        removeLifecycleHooks();
        failure = reason;
        sessionRequest?.abort(failure);
        sessionRequest = undefined;
        channels.clear();
        listeners.clear();
        resetHandlers.clear();
        rejectConnectedWaiters(failure);
        rejectRequests(failure);
        stopped.reject(failure);
        return true;
    }

    async function dispose() {
        if (!shutdown(new Error('Browser page closed')) || !session) return;
        const response = await fetch('/__tt/session/close', {
            method: 'POST', credentials: 'same-origin', keepalive: true,
            headers: { 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken },
            body: JSON.stringify({ sessionId: session.sessionId }),
        });
        if (!response.ok) throw new Error(`Could not close browser page session: HTTP ${response.status}`);
    }

    // The server revokes the login and closes every page of this browser, including this one, so
    // the local stop neither reconnects, reports lost authentication, nor posts a page close.
    async function signOut() {
        if (failure) throw failure;
        const response = await fetch('/__tt/auth/logout', {
            method: 'POST', credentials: 'same-origin', cache: 'no-store',
            headers: { 'content-type': 'application/json' }, body: '{}',
        }).catch(cause => {
            throw new WebConnectionError('Sign out: connection lost; the result is unknown', { cause });
        });
        if (response.status !== 204) throw new Error(`Sign out failed (HTTP ${response.status})`);
        shutdown(new Error('Signed out'));
    }

    function fail(error) {
        if (failure) return;
        failure = error;
        if (error instanceof WebAuthenticationError) {
            epoch += 1;
            resources.reset();
        }
        source?.close();
        source = undefined;
        clearConnectionTimer();
        removeLifecycleHooks();
        sessionRequest?.abort(error);
        sessionRequest = undefined;
        listeners.clear();
        resetHandlers.clear();
        rejectConnectedWaiters(error);
        rejectRequests(error);
        channels.clear();
        stopped.reject(error);
        state('failed');
        onError(error);
    }

    function dispatch(handler, payload) {
        try { Promise.resolve(handler(payload)).catch(onError); }
        catch (error) { onError(error); }
    }

    async function responseBody(response, method, context, signal) {
        try { return await response[method](); }
        catch (cause) {
            signal?.throwIfAborted();
            // Fetch body IO rejects with TypeError; malformed JSON instead rejects with SyntaxError.
            // A received header does not make a later network loss a protocol failure.
            if (cause instanceof TypeError) throw new WebConnectionError(`${context}: response connection lost; outcome may be unknown`, { cause });
            throw new Error(`${context}: invalid ${method === 'json' ? 'JSON' : 'byte'} response (HTTP ${response.status})`, { cause });
        }
    }

    async function responseValue(response, context, signal) {
        // Only the HTTP authentication boundary sets this header, not command authorization errors.
        if (response.status === 401 && response.headers.get('x-tt-login-required') === '1') {
            throw new WebAuthenticationError();
        }
        if (response.status === 204 && response.ok) return null;
        const type = response.headers.get('content-type')?.split(';')[0].trim();
        if (type === 'application/json') {
            const value = await responseBody(response, 'json', context, signal);
            if (!response.ok) {
                if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error(`${context}: invalid JSON error response (HTTP ${response.status})`);
                throw value;
            }
            return value;
        }
        if (response.ok && type === 'application/octet-stream') return new Uint8Array(await responseBody(response, 'arrayBuffer', context, signal));
        if ([502, 503, 504].includes(response.status)) {
            throw new WebConnectionError(`${context}: proxy connection lost (HTTP ${response.status}); outcome may be unknown`);
        }
        throw new Error(`${context}: unexpected response type ${type ?? '<missing>'} (HTTP ${response.status})`);
    }

    function resetSession(reason) {
        source.close();
        source = undefined;
        clearConnectionTimer();
        epoch += 1;
        lastEventId = -1;
        resources.reset();
        channels.clear();
        rejectRequests(new WebSessionResetError());
        starting = undefined;
        session = undefined;
        retryDelay = 1000;
        state('reset');
        // A new page, never a retry of a failed invoke. Declarative consumers rebuild only
        // confirmed registrations; old commits, generations and file handles stay invalid.
        void start().catch(() => {}); // start() reports initialization failure.
        // Cancel old callbacks synchronously; each declarative attach awaits the new ready.
        for (const handler of resetHandlers) dispatch(handler, { reason, epoch });
    }

    function connect(generation, ready) {
        if (generation !== epoch || failure) return;
        const after = lastEventId < 0 ? '' : `&after=${lastEventId}`;
        const events = new EventSource(`/__tt/events?sessionId=${encodeURIComponent(session.sessionId)}${after}`);
        source = events;
        lastActivity = now();
        watchConnection(events, generation);
        let registered = false;
        events.onmessage = ({ data, lastEventId: frameId }) => {
            if (events !== source || generation !== epoch) return;
            try {
                const frame = JSON.parse(data);
                if (frame.type === 'session-reset') {
                    if (!['expired', 'gap'].includes(frame.reason)) throw new Error('Invalid Web session reset reason');
                    resetSession(frame.reason);
                    ready.reject(new WebSessionResetError());
                    return;
                }
                if (frame.type === 'ready') {
                    if (frame.sessionId !== session.sessionId) throw new Error('Web ready frame belongs to another session');
                    registered = true;
                    retryDelay = 1000;
                    lastActivity = now();
                    state('connected');
                    ready.resolve();
                    for (const waiter of connectedWaiters) waiter.resolve();
                    return;
                }
                if (!registered) throw new Error('Web event stream must begin with its session ready frame');
                // Default SSE messages without id inherit the browser's previous lastEventId.
                // Heartbeats are control traffic, not part of PageBus replay or Channel ordering.
                if (frame.type === 'heartbeat') {
                    if (frame.sessionId !== session.sessionId) throw new Error('Web heartbeat belongs to another session');
                    lastActivity = now();
                    return;
                }
                const id = Number(frameId);
                if (typeof frameId !== 'string' || !/^\d+$/.test(frameId) || !Number.isSafeInteger(id)) throw new Error('Web event frame requires a valid SSE id');
                // The cursor records delivery, not execution of an async callback.
                if (id <= lastEventId) return;
                if (id !== lastEventId + 1) throw new Error('Out-of-order Web event frame');
                switch (frame.type) {
                    case 'event':
                        if (typeof frame.event !== 'string' || !Number.isSafeInteger(frame.id) || !Object.hasOwn(frame, 'payload')) throw new Error('Invalid Web event frame');
                        for (const { event, handler, id } of listeners.values()) {
                            if (event === frame.event) dispatch(handler, { event, id, payload: frame.payload });
                        }
                        break;
                    case 'channel': {
                        const entry = channels.get(frame.channelId);
                        if (!entry || frame.sequence !== entry.nextSequence || !Object.hasOwn(frame, 'payload')) throw new Error('Invalid or out-of-order Web Channel frame');
                        entry.nextSequence += 1;
                        dispatch(entry.channel.onmessage, frame.payload);
                        break;
                    }
                    case 'channel-end':
                        if (!channels.delete(frame.channelId)) throw new Error('Unknown Web Channel end frame');
                        break;
                    default:
                        throw new Error(`Unknown Web event frame: ${frame.type}`);
                }
                lastEventId = id;
                lastActivity = now();
            } catch (error) { fail(error); }
        };
        events.onerror = () => {
            if (events !== source || generation !== epoch || failure) return;
            reconnect(false, true); // Also handles native CONNECTING and proxy-refused CLOSED.
        };
    }

    async function openConnection(generation, ready) {
        if (generation !== epoch || failure) return;
        try {
            const creating = !session;
            const controller = sessionRequest = new AbortController();
            // EventSource hides HTTP status. Confirm authentication on the existing recovery path
            // before reopening a stream, including after a sleeping tab missed logout/expiry.
            const response = await fetch(creating ? '/__tt/session' : '/__tt/auth/status', {
                credentials: 'same-origin', cache: 'no-store', headers: { accept: 'application/json' },
                signal: controller.signal,
            }).catch(cause => {
                controller.signal.throwIfAborted();
                throw new WebConnectionError('Web session: connection lost before readiness could be confirmed', { cause });
            });
            const value = await responseValue(response, 'Web session', controller.signal);
            if (generation !== epoch || failure || sessionRequest !== controller) return;
            if (creating) {
                if (typeof value?.sessionId !== 'string' || !value.sessionId || typeof value.csrfToken !== 'string' || !value.csrfToken) throw new Error('Web session requires sessionId and csrfToken');
                session = value;
            } else {
                if (typeof value?.authenticated !== 'boolean') throw new Error('Web authentication status requires authenticated');
                if (!value.authenticated) throw new WebAuthenticationError();
            }
            sessionRequest = undefined;
            connect(generation, ready);
        } catch (error) {
            if (generation !== epoch || failure) return;
            sessionRequest = undefined;
            // Session creation has no command side effects. Initial load and session replacement
            // use the same network-only recovery path as SSE; protocol/auth/domain errors stop it.
            if (isWebConnectionError(error)) reconnect();
            else fail(error);
        }
    }

    function start() {
        if (failure) return Promise.reject(failure);
        if (starting) return starting;
        const ready = initialReady = Promise.withResolvers();
        starting = Promise.race([ready.promise, stopped.promise]);
        void openConnection(epoch, ready);
        return starting;
    }

    function waitUntilConnected({ signal } = {}) {
        if (signal?.aborted) return Promise.reject(signal.reason);
        if (failure) return Promise.reject(failure);
        if (connectionState === 'connected') return Promise.resolve();
        const pending = Promise.withResolvers();
        const cleanup = () => {
            connectedWaiters.delete(waiter);
            signal?.removeEventListener('abort', abort);
        };
        const waiter = {
            resolve() { cleanup(); pending.resolve(); },
            reject(error) { cleanup(); pending.reject(error); },
        };
        const abort = () => waiter.reject(signal.reason);
        connectedWaiters.add(waiter);
        signal?.addEventListener('abort', abort, { once: true });
        // Waiters survive legitimate session replacement; terminal paths reject them centrally.
        // Do not retain a cancelled waiter in the shared initialization promise's handlers.
        void start().catch(() => {});
        return pending.promise;
    }

    function recoverConnection() {
        if (failure) throw failure;
        if (starting) reconnect(true);
        else void start().catch(() => {}); // Initialization reports its own failure.
    }

    function createChannel(onmessage) {
        if (failure) throw failure;
        if (typeof onmessage !== 'function') throw new TypeError('A Web Channel handler is required');
        const id = String(++nextChannelId);
        const channel = {
            onmessage,
            toJSON() {
                if (channels.get(id)?.channel !== channel) throw new Error('Web Channel is no longer active');
                return { $channel: id };
            },
        };
        channels.set(id, { channel, nextSequence: 0 });
        return channel;
    }

    async function invoke(command, args = {}, options = {}) {
        if (typeof command !== 'string' || !/^[a-z][a-z0-9_]*$/.test(command)) throw new TypeError(`Unsupported Web command: ${command}. Tauri plugins are not available in a browser.`);
        await start();
        if (failure) throw failure;
        const generation = epoch;
        const headers = new Headers(options.headers);
        for (const key of ['x-tt-command', 'x-tt-session', 'x-tt-csrf', 'content-type', 'content-encoding']) {
            if (headers.has(key)) throw new TypeError(`Reserved Web transport header: ${key}`);
        }
        headers.set('x-tt-csrf', session.csrfToken);
        headers.set('accept', 'application/json, application/octet-stream');
        const binary = args instanceof ArrayBuffer || ArrayBuffer.isView(args);
        let body;
        if (binary) {
            headers.set('content-type', 'application/octet-stream');
            headers.set('x-tt-command', command);
            headers.set('x-tt-session', session.sessionId);
            body = args instanceof ArrayBuffer ? args : new Uint8Array(args.buffer, args.byteOffset, args.byteLength);
        } else {
            headers.set('content-type', 'application/json');
            body = JSON.stringify({ sessionId: session.sessionId, command, args: resources.argumentsFor(command, args) });
        }
        const controller = new AbortController();
        const cancelled = Promise.withResolvers();
        const abort = () => { controller.abort(options.signal.reason); cancelled.reject(options.signal.reason); };
        options.signal?.throwIfAborted();
        options.signal?.addEventListener('abort', abort, { once: true });
        const request = { controller, reject: cancelled.reject };
        requests.add(request);
        try {
            const value = await Promise.race([
                (async () => {
                    if (binary && options.compressible && body.byteLength >= GZIP_MIN_BYTES) {
                        try { body = await gzip(body); }
                        catch (cause) { throw new Error(`Web invoke ${command}: request compression failed; command was not sent`, { cause }); }
                        headers.set('content-encoding', 'gzip');
                    }
                    // Preparation can yield to page exit, cancellation or session replacement.
                    // Never send its old frame after that transition, even if compression finishes.
                    controller.signal.throwIfAborted();
                    if (generation !== epoch) throw new WebSessionResetError();
                    return fetch('/__tt/invoke', { method: 'POST', credentials: 'same-origin', headers, body, signal: controller.signal })
                        .catch(cause => {
                            controller.signal.throwIfAborted();
                            throw new WebConnectionError(`Web invoke ${command}: connection lost; outcome may be unknown, command was not retried`, { cause });
                        })
                        .then(response => responseValue(response, `Web invoke ${command}`, controller.signal));
                })(),
                cancelled.promise,
            ]);
            if (generation !== epoch) throw new WebSessionResetError();
            if (command === 'close_resource') resources.release(args.rid);
            return resources.accept(command, value);
        } catch (error) {
            if (error instanceof WebAuthenticationError) fail(error);
            throw error;
        } finally {
            requests.delete(request);
            options.signal?.removeEventListener('abort', abort);
        }
    }

    async function listen(event, handler) {
        if (typeof event !== 'string' || !event || typeof handler !== 'function') throw new TypeError('An event name and handler are required');
        const id = ++nextListenerId;
        listeners.set(id, { event, handler, id });
        try { await start(); }
        catch (error) { listeners.delete(id); throw error; }
        return () => { listeners.delete(id); };
    }

    return {
        start, invoke, listen, createChannel, dispose, signOut, waitUntilConnected, recoverConnection,
        close: () => fail(new Error('Web transport closed')),
        async csrfToken() { await start(); return session.csrfToken; },
        sessionEpoch: () => epoch,
        onSessionReset(handler) {
            resetHandlers.add(handler);
            return () => resetHandlers.delete(handler);
        },
    };
}
