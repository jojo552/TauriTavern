// Explicit browser transport for the Rust HTTP host. This is not a Tauri runtime shim.
import { createWebResources } from './resources.js';

// Avoid compression overhead for small frames. Only the byte owner marks text as compressible;
// opaque staging uploads and extension-store blobs retain their raw representation.
const GZIP_MIN_BYTES = 32 * 1024;

async function gzip(bytes) {
    const stream = new Blob([bytes]).stream().pipeThrough(new CompressionStream('gzip'));
    return new Uint8Array(await new Response(stream).arrayBuffer());
}

export function createWebTransport({
    fetch = globalThis.fetch?.bind(globalThis),
    EventSource = globalThis.EventSource,
    onError = error => globalThis.reportError ? globalThis.reportError(error) : console.error(error),
    onState = () => {},
    schedule = globalThis.setTimeout,
    unschedule = globalThis.clearTimeout,
} = {}) {
    if (typeof fetch !== 'function' || typeof EventSource !== 'function') throw new Error('Web transport requires fetch and EventSource');
    let session, source, starting, failure, reconnectTimer;
    let epoch = 0, nextChannelId = 0, nextListenerId = 0, lastEventId = -1, retryDelay = 1000;
    let connectionState;
    const resources = createWebResources();
    const channels = new Map(), listeners = new Map();
    const resetHandlers = new Set(), requests = new Set();
    const stopped = Promise.withResolvers();
    stopped.promise.catch(() => {});

    function state(value) {
        if (connectionState === value) return;
        connectionState = value;
        onState(value);
    }

    function rejectRequests(error) {
        for (const request of requests) {
            request.controller.abort(error);
            request.reject(error);
        }
    }

    async function dispose() {
        source?.close();
        source = undefined;
        unschedule(reconnectTimer);
        failure = new Error('Browser page closed');
        rejectRequests(failure);
        stopped.reject(failure);
        if (!session) return;
        const response = await fetch('/__tt/session/close', {
            method: 'POST', credentials: 'same-origin', keepalive: true,
            headers: { 'content-type': 'application/json', 'x-tt-csrf': session.csrfToken },
            body: JSON.stringify({ sessionId: session.sessionId }),
        });
        if (!response.ok) throw new Error(`Could not close browser page session: HTTP ${response.status}`);
    }

    function fail(error) {
        if (failure) return;
        failure = error;
        source?.close();
        unschedule(reconnectTimer);
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

    async function responseValue(response, context) {
        if (response.status === 204 && response.ok) return null;
        const type = response.headers.get('content-type')?.split(';')[0].trim();
        if (type === 'application/json') {
            let value;
            try { value = await response.json(); }
            catch (cause) { throw new Error(`${context}: invalid JSON response (HTTP ${response.status})`, { cause }); }
            if (!response.ok) throw value;
            return value;
        }
        if (response.ok && type === 'application/octet-stream') return new Uint8Array(await response.arrayBuffer());
        throw new Error(`${context}: unexpected response type ${type ?? '<missing>'} (HTTP ${response.status})`);
    }

    function resetSession(reason) {
        source.close();
        source = undefined;
        unschedule(reconnectTimer);
        epoch += 1;
        lastEventId = -1;
        resources.reset();
        channels.clear();
        rejectRequests(new Error('Server page session was reset; pending command outcomes may be unknown. Old files and cold swipe sources must be reopened.'));
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
        let registered = false;
        events.onmessage = ({ data, lastEventId: frameId }) => {
            if (events !== source || generation !== epoch) return;
            try {
                const frame = JSON.parse(data);
                if (frame.type === 'session-reset') {
                    if (!['expired', 'gap'].includes(frame.reason)) throw new Error('Invalid Web session reset reason');
                    resetSession(frame.reason);
                    ready.reject(new Error('Web session was replaced'));
                    return;
                }
                if (frame.type === 'ready') {
                    if (frame.sessionId !== session.sessionId) throw new Error('Web ready frame belongs to another session');
                    registered = true;
                    retryDelay = 1000;
                    state('connected');
                    ready.resolve();
                    return;
                }
                if (!registered) throw new Error('Web event stream must begin with its session ready frame');
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
            } catch (error) { fail(error); }
        };
        events.onerror = () => {
            if (events !== source || generation !== epoch || failure) return;
            events.close(); // One reconnect path for both native CONNECTING and proxy-refused CLOSED.
            source = undefined;
            state('reconnecting');
            reconnectTimer = schedule(() => connect(generation, ready), retryDelay);
            retryDelay = Math.min(10_000, retryDelay * 2);
            // HTTP requests have their own outcomes. Don't abort a save its HTTP response can
            // still confirm, and don't replay a request whose outcome was lost.
        };
    }

    function start() {
        if (failure) return Promise.reject(failure);
        if (starting) return starting;
        const generation = epoch;
        starting = (async () => {
            const created = await responseValue(await fetch('/__tt/session', {
                credentials: 'same-origin', cache: 'no-store', headers: { accept: 'application/json' },
            }), 'Web session');
            if (typeof created?.sessionId !== 'string' || !created.sessionId || typeof created.csrfToken !== 'string' || !created.csrfToken) throw new Error('Web session requires sessionId and csrfToken');
            if (failure) throw failure;
            if (generation !== epoch) throw new Error('Web session initialization was superseded');
            session = created;
            const ready = Promise.withResolvers();
            connect(generation, ready);
            await Promise.race([ready.promise, stopped.promise]);
        })().catch(error => {
            if (generation === epoch) fail(error);
            throw error;
        });
        return starting;
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
                    if (generation !== epoch) throw new Error('Request belongs to a previous server page session');
                    return fetch('/__tt/invoke', { method: 'POST', credentials: 'same-origin', headers, body, signal: controller.signal })
                        .catch(cause => { throw new Error(`Web invoke ${command}: connection lost; outcome may be unknown, command was not retried`, { cause }); })
                        .then(response => responseValue(response, `Web invoke ${command}`));
                })(),
                cancelled.promise,
            ]);
            if (generation !== epoch) throw new Error('Response belongs to a previous server page session');
            if (command === 'close_resource') resources.release(args.rid);
            return resources.accept(command, value);
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
        start, invoke, listen, createChannel, dispose,
        close: () => fail(new Error('Web transport closed')),
        async csrfToken() { await start(); return session.csrfToken; },
        sessionEpoch: () => epoch,
        onSessionReset(handler) {
            resetHandlers.add(handler);
            return () => resetHandlers.delete(handler);
        },
    };
}
