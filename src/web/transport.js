// Explicit browser transport for the Rust HTTP host. This is not a Tauri runtime shim.
export function createWebTransport({
    fetch = globalThis.fetch?.bind(globalThis),
    EventSource = globalThis.EventSource,
    onError = error => globalThis.reportError ? globalThis.reportError(error) : console.error(error),
} = {}) {
    if (typeof fetch !== 'function' || typeof EventSource !== 'function') {
        throw new Error('Web transport requires fetch and EventSource');
    }
    let session;
    let source;
    let starting;
    let failure;
    let nextChannelId = 0;
    let nextListenerId = 0;
    const channels = new Map();
    const listeners = new Map();
    const requests = new Set();
    const stopped = Promise.withResolvers();
    // Failure is also delivered to pending callers. It must not become an unhandled rejection
    // when there are currently no calls (onError is the visible notification in that case).
    stopped.promise.catch(() => {});

    function fail(error) {
        if (failure) return;
        failure = error;
        source?.close(); // Never allow EventSource's implicit reconnect/replay.
        for (const controller of requests) controller.abort(error);
        channels.clear();
        listeners.clear();
        stopped.reject(error);
        onError(error);
    }

    function dispatch(handler, payload) {
        try {
            Promise.resolve(handler(payload)).catch(onError);
        } catch (error) {
            onError(error);
        }
    }

    async function responseValue(response, context) {
        if (response.status === 204 && response.ok) return null;
        const type = response.headers.get('content-type')?.split(';')[0].trim();
        if (type === 'application/json') {
            let value;
            try {
                value = await response.json();
            } catch (cause) {
                throw new Error(`${context}: invalid JSON response (HTTP ${response.status})`, { cause });
            }
            if (!response.ok) throw value; // Preserve the Rust CommandError, including structured details.
            return value;
        }
        if (response.ok && type === 'application/octet-stream') {
            return new Uint8Array(await response.arrayBuffer());
        }
        throw new Error(`${context}: unexpected response type ${type ?? '<missing>'} (HTTP ${response.status})`);
    }

    function start() {
        if (failure) return Promise.reject(failure);
        if (starting) return starting;
        starting = (async () => {
            session = await responseValue(await fetch('/__tt/session', {
                credentials: 'same-origin', cache: 'no-store', headers: { accept: 'application/json' },
            }), 'Web session');
            if (typeof session?.sessionId !== 'string' || !session.sessionId
                || typeof session.csrfToken !== 'string' || !session.csrfToken) {
                throw new Error('Web session requires sessionId and csrfToken');
            }
            if (failure) throw failure;
            const ready = Promise.withResolvers();
            let registered = false;
            source = new EventSource(`/__tt/events?sessionId=${encodeURIComponent(session.sessionId)}`);
            source.onmessage = ({ data }) => {
                try {
                    const frame = JSON.parse(data);
                    if (!registered) {
                        if (frame.type !== 'ready' || frame.sessionId !== session.sessionId) {
                            throw new Error('Web event stream must begin with its session ready frame');
                        }
                        registered = true;
                        ready.resolve();
                        return;
                    }
                    switch (frame.type) {
                        case 'event': {
                            if (typeof frame.event !== 'string' || !Number.isSafeInteger(frame.id)
                                || !Object.hasOwn(frame, 'payload')) throw new Error('Invalid Web event frame');
                            for (const { event, handler, id } of listeners.values()) {
                                if (event === frame.event) dispatch(handler, { event, id, payload: frame.payload });
                            }
                            break;
                        }
                        case 'channel': {
                            const state = channels.get(frame.channelId);
                            if (!state || frame.sequence !== state.nextSequence
                                || !Object.hasOwn(frame, 'payload')) throw new Error('Invalid or out-of-order Web Channel frame');
                            state.nextSequence += 1;
                            dispatch(state.channel.onmessage, frame.payload);
                            break;
                        }
                        case 'channel-end':
                            if (!channels.delete(frame.channelId)) throw new Error('Unknown Web Channel end frame');
                            break;
                        case 'error':
                            throw new Error('Web event stream failed', { cause: frame.error });
                        case 'close':
                            throw new Error('Web event stream closed by host');
                        default:
                            throw new Error(`Unknown Web event frame: ${frame.type}`);
                    }
                } catch (error) {
                    fail(error);
                }
            };
            source.onerror = () => fail(new Error('Web event stream disconnected; pending command outcomes may be unknown. Reload to reconnect.'));
            await Promise.race([ready.promise, stopped.promise]);
        })().catch(error => {
            fail(error);
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
        if (typeof command !== 'string' || !/^[a-z][a-z0-9_]*$/.test(command)) {
            throw new TypeError(`Unsupported Web command: ${command}. Tauri plugins are not available in a browser.`);
        }
        await start();
        if (failure) throw failure;
        const headers = new Headers(options.headers);
        for (const key of ['x-tt-command', 'x-tt-session', 'x-tt-csrf', 'content-type']) {
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
            body = JSON.stringify({ sessionId: session.sessionId, command, args });
        }
        const controller = new AbortController();
        const abort = () => controller.abort(options.signal.reason);
        options.signal?.throwIfAborted();
        options.signal?.addEventListener('abort', abort, { once: true });
        requests.add(controller);
        try {
            return await Promise.race([
                fetch('/__tt/invoke', { method: 'POST', credentials: 'same-origin', headers, body, signal: controller.signal })
                    .then(response => responseValue(response, `Web invoke ${command}`)),
                stopped.promise,
            ]);
        } finally {
            requests.delete(controller);
            options.signal?.removeEventListener('abort', abort);
        }
    }

    async function listen(event, handler) {
        if (typeof event !== 'string' || !event || typeof handler !== 'function') {
            throw new TypeError('An event name and handler are required');
        }
        const id = ++nextListenerId;
        listeners.set(id, { event, handler, id });
        try {
            await start();
        } catch (error) {
            listeners.delete(id);
            throw error;
        }
        return () => { listeners.delete(id); };
    }

    return {
        start, invoke, listen, createChannel,
        close: () => fail(new Error('Web transport closed')),
        async csrfToken() { await start(); return session.csrfToken; },
    };
}
