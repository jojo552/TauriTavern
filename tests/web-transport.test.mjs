import test from 'node:test';
import assert from 'node:assert/strict';
import { createWebTransport, isWebConnectionError, isWebSessionResetError, WebAuthenticationError } from '../src/web/transport.js';
import { createAgentRunLiveSubscribe } from '../src/tauri/main/api/agent-run-live-subscription.js';
import { gunzipSync } from 'node:zlib';
import { installHostIdentity } from './helpers/host-identity.mjs';
import { createRouteRegistry } from '../src/tauri/main/router.js';
import { registerSettingsRoutes } from '../src/tauri/main/routes/settings-routes.js';
import { jsonResponse } from '../src/tauri/main/http-utils.js';

function harness() {
    const calls = [], errors = [], streams = [], timers = new Map();
    let clock = 0, nextTimer = 0;
    const window = new EventTarget(), document = new EventTarget();
    document.visibilityState = 'visible';
    function emit(target, type, properties = {}) {
        target.dispatchEvent(Object.assign(new Event(type), properties));
    }
    function runNextTimer() {
        const [id, timer] = [...timers].sort((a, b) => a[1].at - b[1].at)[0];
        timers.delete(id);
        clock = Math.max(clock, timer.at);
        timer.callback();
    }
    function advance(ms) {
        const until = clock + ms;
        while ([...timers.values()].some(timer => timer.at <= until)) runNextTimer();
        clock = until;
    }
    const streamReady = Promise.withResolvers();
    let reply = () => Response.json(null);
    let authReply = () => Response.json({ authenticated: true });
    let sessionReply = (init, created) => Response.json({ sessionId: `session-${created}`, csrfToken: `csrf-${created}` });
    let created = 0;
    let nextStream;
    class Events {
        constructor(url) { this.url = url; this.nextId = 0; this.lastId = ''; streams.push(this); streamReady.resolve(this); nextStream?.resolve(this); }
        close() { this.closed = true; }
        frame(frame, id) {
            if (!['ready', 'session-reset', 'heartbeat'].includes(frame.type)) this.lastId = id ?? String(this.nextId++);
            this.onmessage({ data: JSON.stringify(frame), lastEventId: this.lastId });
        }
    }
    const transport = createWebTransport({
        EventSource: Events,
        onError: error => errors.push(error),
        now: () => clock, window, document,
        schedule: (callback, delay) => { const id = ++nextTimer; timers.set(id, { callback, at: clock + delay }); return id; },
        unschedule: id => timers.delete(id),
        fetch: async (url, init) => {
            calls.push({ url, ...init });
            if (url === '/__tt/auth/status') return authReply(init);
            if (url === '/__tt/session') {
                created++;
                return sessionReply(init, created);
            }
            return reply(init);
        },
    });
    async function ready() {
        const start = transport.start();
        const stream = await streamReady.promise;
        stream.frame({ type: 'ready', sessionId: 'session-1' });
        await start;
        return stream;
    }
    async function reset(stream) {
        nextStream = Promise.withResolvers();
        stream.frame({ type: 'session-reset', reason: 'expired' });
        // Deterministic synchronization: wait for creation, then send this connection's ready.
        const started = transport.start();
        const next = await nextStream.promise;
        next.frame({ type: 'ready', sessionId: `session-${created}` });
        await started;
        return next;
    }
    async function resume() {
        nextStream = Promise.withResolvers();
        runNextTimer();
        return nextStream.promise;
    }
    return { transport, calls, errors, streams, timers, reset, resume, streamReady: streamReady.promise, ready,
        authReply: handler => { authReply = handler; },
        window, document, emit, advance, runNextTimer, jump: ms => { clock += ms; },
        reply: handler => { reply = handler; }, sessionReply: handler => { sessionReply = handler; } };
}

test('live projection receives replay after the HTTP subscription finishes, until its Channel ends', async () => {
    const h = harness();
    const stream = await h.ready();
    const entered = Promise.withResolvers(), confirmed = Promise.withResolvers();
    h.reply(init => { entered.resolve(JSON.parse(init.body)); return confirmed.promise; });
    const updates = [];
    const subscribe = createAgentRunLiveSubscribe({
        safeInvoke: h.transport.invoke, channelFactory: h.transport.createChannel,
    });
    const unsubscribe = subscribe('run-live', update => updates.push(update));
    const request = await entered.promise;
    stream.onerror();
    confirmed.resolve(Response.json(null));
    await new Promise(resolve => setImmediate(resolve));
    const resumed = await h.resume();
    resumed.frame({ type: 'ready', sessionId: 'session-1' });
    const snapshot = { type: 'snapshot', calls: [], responses: [] };
    resumed.frame({ type: 'channel', channelId: request.args.channel.$channel, sequence: 0, payload: snapshot }, '0');
    resumed.frame({ type: 'channel-end', channelId: request.args.channel.$channel }, '1');
    assert.deepEqual(updates, [snapshot]);
    assert.deepEqual(h.errors, []);
    assert.equal(h.calls.filter(call => call.url === '/__tt/invoke').length, 1, 'never repeats the subscribe');
    unsubscribe();
});

test('invoke waits for ready, keeps DTOs intact and sends same-origin session CSRF', async () => {
    const h = harness();
    h.reply(() => Response.json({ name: 'Alice', data: [1, 2] }));
    const result = h.transport.invoke('get_character', { character_id: 'Alice', dto: { camelCase: 3 } });
    const stream = await h.streamReady;
    assert.equal(h.calls.length, 1);
    stream.frame({ type: 'ready', sessionId: 'session-1' });
    assert.deepEqual(await result, { name: 'Alice', data: [1, 2] });
    assert.deepEqual(JSON.parse(h.calls[1].body), { sessionId: 'session-1', command: 'get_character', args: { character_id: 'Alice', dto: { camelCase: 3 } } });
    assert.equal(h.calls[1].headers.get('x-tt-csrf'), 'csrf-1');
    assert.equal(h.calls[1].headers.get('content-type'), 'application/json');
    assert.equal(h.calls[1].credentials, 'same-origin');
});

test('session initialization retries only network loss on the shared backoff and still waits for ready before invoking', async () => {
    const h = harness();
    const release = Promise.withResolvers();
    h.sessionReply((init, attempt) => {
        if (attempt === 1) throw new TypeError('offline');
        if (attempt === 2) return new Response('proxy unavailable', { status: 503 });
        if (attempt === 3) return new Response(new ReadableStream({
            start(controller) { controller.error(new TypeError('response connection lost')); },
        }), { headers: { 'content-type': 'application/json' } });
        return release.promise;
    });
    const started = h.transport.start();
    const connected = h.transport.waitUntilConnected();
    const invoking = h.transport.invoke('save_settings');
    let due = 0;
    for (const delay of [1000, 2000, 4000]) {
        await new Promise(resolve => setImmediate(resolve));
        assert.equal(h.timers.size, 1);
        due += delay;
        assert.equal([...h.timers.values()][0].at, due);
        assert.deepEqual(h.errors, []);
        h.runNextTimer();
    }
    // Online/explicit recovery while GET is in flight joins it, not another session or timer.
    h.emit(h.window, 'online');
    h.transport.recoverConnection();
    assert.equal(h.calls.length, 4);
    assert.equal(h.timers.size, 0);
    release.resolve(Response.json({ sessionId: 'session-4', csrfToken: 'csrf-4' }));
    const stream = await h.streamReady;
    assert.equal(h.calls.length, 4, 'no command is sent before the first ready');
    stream.frame({ type: 'ready', sessionId: 'session-4' });
    await Promise.all([started, connected, invoking]);
    assert.equal(h.calls.filter(call => call.url === '/__tt/invoke').length, 1);
    assert.equal(h.calls.at(-1).headers.get('x-tt-csrf'), 'csrf-4');
    assert.deepEqual(h.errors, []);
    assert.equal(h.timers.size, 1);
});

test('session protocol and authentication errors fail pending readiness instead of becoming reconnect loops', async () => {
    const cases = [
        [() => Response.json({ sessionId: '', csrfToken: 'invalid' }), /requires sessionId/],
        [() => new Response('{', { headers: { 'content-type': 'application/json' } }), /invalid JSON/],
        [() => new Response('authentication required', { status: 401 }), /HTTP 401/],
        [() => Response.json(null, { status: 503 }), /invalid JSON error response/],
        [() => Response.json({ Unauthorized: 'expired auth' }, { status: 503 }), error => {
            assert.deepEqual(error, { Unauthorized: 'expired auth' });
            return true;
        }],
    ];
    for (const [reply, expected] of cases) {
        const h = harness();
        h.sessionReply(reply);
        const started = assert.rejects(h.transport.start(), expected);
        const connected = assert.rejects(h.transport.waitUntilConnected(), expected);
        await Promise.all([started, connected]);
        assert.equal(isWebConnectionError(h.errors[0]), false);
        assert.equal(h.errors.length, 1);
        assert.equal(h.timers.size, 0);
        h.emit(h.window, 'online');
        assert.equal(h.calls.length, 1);
        assert.throws(() => h.transport.recoverConnection(), expected);
    }
});

test('same-page reconnect resumes channels exactly once without cancelling or replaying a confirmed HTTP save', async () => {
    const h = harness();
    const values = [], events = [];
    const channel = h.transport.createChannel(value => values.push(value));
    const stream = await h.ready();
    const unlisten = await h.transport.listen('change', event => events.push(event.payload));
    await h.transport.invoke('register_agent_extension_tool', { definition: { name: 'inspect' }, channel });
    const channelId = JSON.parse(h.calls[1].body).args.channel.$channel;
    stream.frame({ type: 'channel', channelId, sequence: 0, payload: 'first' }, '0');
    const entered = Promise.withResolvers(), completed = Promise.withResolvers();
    h.reply(init => { entered.resolve(init); return completed.promise; });
    const save = h.transport.invoke('save_settings', { data: 'test-only' });
    const request = await entered.promise;
    stream.onerror();
    assert.equal(stream.closed, true);
    assert.equal(request.signal.aborted, false, 'SSE loss is not HTTP save failure');
    completed.resolve(Response.json({ ok: true }));
    assert.deepEqual(await save, { ok: true });
    const before = h.calls.filter(call => call.url === '/__tt/invoke').length;
    const resumed = await h.resume();
    assert.match(resumed.url, /sessionId=session-1&after=0/);
    resumed.frame({ type: 'ready', sessionId: 'session-1' });
    resumed.frame({ type: 'channel', channelId, sequence: 0, payload: 'duplicate' }, '0');
    resumed.frame({ type: 'channel', channelId, sequence: 1, payload: 'second' }, '1');
    resumed.frame({ type: 'event', event: 'change', id: 7, payload: 'yes' }, '2');
    unlisten();
    resumed.frame({ type: 'event', event: 'change', id: 8, payload: 'no' }, '3');
    assert.deepEqual(values, ['first', 'second']);
    assert.deepEqual(events, ['yes']);
    assert.equal(h.calls.filter(call => call.url === '/__tt/invoke').length, before, 'never replays the save');
    assert.deepEqual(h.errors, []);
    resumed.frame({ type: 'channel-end', channelId }, '4');
    assert.throws(() => JSON.stringify(channel), /no longer active/);
});

test('failed HTTP commands report unknown outcomes once; later requests are still usable', async () => {
    const h = harness();
    await h.ready();
    h.reply(() => { throw new TypeError('network failed'); });
    await assert.rejects(h.transport.invoke('save_settings'), error => {
        assert.equal(isWebConnectionError(error), true);
        assert.match(error.message, /outcome may be unknown.*not retried/);
        return true;
    });
    assert.equal(h.calls.length, 2);
    h.transport.recoverConnection();
    h.transport.recoverConnection();
    const connected = h.transport.waitUntilConnected();
    const resumed = await h.resume();
    resumed.frame({ type: 'ready', sessionId: 'session-1' });
    await connected;
    assert.equal(h.calls.filter(call => call.url === '/__tt/invoke').length, 1, 'recovery never resends the unknown save');
    h.reply(() => Response.json(true));
    assert.equal(await h.transport.invoke('is_ready'), true);
    assert.equal(h.calls.filter(call => call.url === '/__tt/invoke').length, 2);
});

test('lost authentication stops HTTP and sleeping-tab recovery without replaying writes or reopening SSE', async () => {
    for (const loss of ['http', 'sleep']) {
        const h = harness();
        const stream = await h.ready();
        if (loss === 'http') {
            h.reply(() => Response.json({ error: 'Login required' }, { status: 401, headers: { 'x-tt-login-required': '1' } }));
            await assert.rejects(h.transport.invoke('save_settings'), WebAuthenticationError);
        } else {
            h.authReply(() => Response.json({ authenticated: false }));
            h.jump(90_000);
            h.emit(h.document, 'visibilitychange');
            const failed = assert.rejects(h.transport.waitUntilConnected(), WebAuthenticationError);
            h.runNextTimer();
            await failed;
        }
        assert.equal(stream.closed, true);
        assert.equal(h.errors.length, 1);
        assert.equal(h.timers.size, 0);
        assert.equal(h.streams.length, 1);
        assert.equal(h.transport.sessionEpoch(), 1);
        const before = h.calls.length;
        await assert.rejects(h.transport.invoke('save_settings'), WebAuthenticationError);
        h.emit(h.window, 'online');
        stream.onerror();
        assert.equal(h.calls.length, before, 'no replay or auth retry after authentication is known lost');
    }
});

test('wake signals keep healthy SSE, merge stale recovery, and fence late frames and cancelled timers', async () => {
    const h = harness();
    const values = [];
    const channel = h.transport.createChannel(value => values.push(value));
    const { $channel: channelId } = channel.toJSON();
    const stream = await h.ready();
    stream.frame({ type: 'channel', channelId, sequence: 0, payload: 'first' }, '0');
    const wake = () => {
        h.emit(h.document, 'visibilitychange');
        h.emit(h.window, 'online');
        h.emit(h.window, 'pageshow', { persisted: true });
    };
    wake();
    assert.equal(stream.closed, undefined, 'foreground does not break a healthy stream');
    assert.equal(h.streams.length, 1);
    const oldTimer = [...h.timers.values()][0].callback;
    h.jump(90_000); // Mobile suspension: wall time advances without running queued timers.
    wake();
    h.transport.recoverConnection();
    assert.equal(h.timers.size, 1);
    oldTimer(); // It may already be queued despite clearTimeout.
    assert.equal(h.streams.length, 1);
    const connected = h.transport.waitUntilConnected();
    const resumed = await h.resume();
    wake();
    h.transport.recoverConnection();
    stream.onerror();
    stream.frame({ type: 'channel', channelId, sequence: 1, payload: 'late old' }, '1');
    stream.frame({ type: 'heartbeat', sessionId: 'wrong-old-session' });
    stream.frame({ type: 'session-reset', reason: 'expired' });
    assert.equal(h.streams.length, 2);
    resumed.frame({ type: 'ready', sessionId: 'session-1' });
    await connected;
    resumed.frame({ type: 'channel', channelId, sequence: 1, payload: 'second' }, '1');
    assert.deepEqual(values, ['first', 'second']);
    assert.equal(h.transport.sessionEpoch(), 0);
    assert.deepEqual(h.errors, []);
    assert.equal(h.timers.size, 1);
});

test('observable heartbeats maintain liveness without advancing the data cursor or Channel sequence', async () => {
    const h = harness();
    const values = [];
    const channel = h.transport.createChannel(value => values.push(value));
    const { $channel: channelId } = channel.toJSON();
    const stream = await h.ready();
    h.advance(80_000);
    stream.frame({ type: 'heartbeat', sessionId: 'session-1' });
    stream.frame({ type: 'channel', channelId, sequence: 0, payload: 'first' }, '0');
    h.advance(10_000); // Original watchdog checks the newer activity rather than disconnecting.
    assert.equal(stream.closed, undefined);
    assert.equal(h.timers.size, 1);
    stream.frame({ type: 'heartbeat', sessionId: 'session-1' }); // Inherits SSE lastEventId=0.
    h.advance(90_000);
    assert.equal(stream.closed, true, 'silent half-open SSE is actively recovered');
    const resumed = await h.resume();
    assert.equal(new URL(resumed.url, 'http://host').searchParams.get('after'), '0');
    resumed.frame({ type: 'ready', sessionId: 'session-1' });
    resumed.frame({ type: 'channel', channelId, sequence: 1, payload: 'second' }, '1');
    assert.deepEqual(values, ['first', 'second']);
    assert.deepEqual(h.errors, []);
});

test('connected waiters cancel independently and surviving waiters follow replacement session ready', async () => {
    const h = harness();
    const stream = await h.ready();
    stream.onerror();
    const controller = new AbortController();
    const cancelled = h.transport.waitUntilConnected({ signal: controller.signal });
    const reason = new Error('reader cancelled');
    const rejected = assert.rejects(cancelled, error => error === reason);
    const connected = h.transport.waitUntilConnected();
    controller.abort(reason);
    await rejected;
    const resumed = await h.resume();
    h.sessionReply((init, attempt) => {
        if (attempt === 2) throw new TypeError('offline while replacing session');
        return Response.json({ sessionId: `session-${attempt}`, csrfToken: `csrf-${attempt}` });
    });
    const replaced = h.reset(resumed);
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(h.timers.size, 1, 'replacement GET network loss remains recoverable');
    const staleTimer = [...h.timers.values()][0].callback;
    h.emit(h.window, 'online');
    h.transport.recoverConnection();
    staleTimer();
    assert.equal(h.calls.filter(call => call.url === '/__tt/session').length, 2);
    h.runNextTimer();
    await replaced;
    await connected;
    assert.equal(h.transport.sessionEpoch(), 1);
    await h.transport.waitUntilConnected();
    await assert.rejects(h.transport.waitUntilConnected({ signal: controller.signal }), error => error === reason);
    assert.deepEqual(h.errors, []);
});

test('invoke cancellation is not a transport failure and never causes reconnect or HTTP replay', async () => {
    const h = harness();
    const stream = await h.ready();
    const controller = new AbortController(), entered = Promise.withResolvers();
    h.reply(init => new Promise((resolve, reject) => {
        entered.resolve();
        init.signal.addEventListener('abort', () => reject(new TypeError('fetch aborted')), { once: true });
    }));
    const saving = h.transport.invoke('save_settings', {}, { signal: controller.signal });
    const rejected = assert.rejects(saving, error => {
        assert.equal(isWebConnectionError(error), false);
        assert.equal(error, controller.signal.reason);
        return true;
    });
    await entered.promise;
    controller.abort();
    await rejected;
    assert.equal(stream.closed, undefined);
    assert.equal(h.calls.length, 2);
    assert.deepEqual(h.errors, []);
});

test('new server session rejects pending work, changes CSRF and invalidates old resource handles without aliasing new readers', async () => {
    const h = harness();
    const stream = await h.ready();
    h.reply(() => Response.json(1));
    const oldReader = await h.transport.invoke('open_chat_reader', { target: {} });
    const entered = Promise.withResolvers();
    h.reply(init => { entered.resolve(init); return new Promise(() => {}); });
    const unknown = h.transport.invoke('save_settings');
    const rejected = assert.rejects(unknown, error => isWebSessionResetError(error));
    const request = await entered.promise;
    const notices = [];
    h.transport.onSessionReset(notice => notices.push(notice));
    await h.reset(stream);
    await rejected;
    assert.equal(request.signal.aborted, true);
    // The old source cannot deliver to or close the replacement connection.
    stream.onerror();
    stream.frame({ type: 'session-reset', reason: 'expired' });
    const before = h.calls.length;
    await assert.rejects(h.transport.invoke('read_bytes', { rid: oldReader }), /resource expired/);
    await assert.rejects(h.transport.invoke('close_resource', { rid: oldReader }), /resource expired/);
    assert.equal(h.calls.length, before);
    h.reply(() => Response.json(1)); // Same server id as the old reader.
    const nextReader = await h.transport.invoke('open_chat_reader', { target: {} });
    assert.notEqual(nextReader, oldReader);
    h.reply(() => new Response(new Uint8Array([0, 255]), { headers: { 'content-type': 'application/octet-stream' } }));
    assert.deepEqual(await h.transport.invoke('read_bytes', { rid: nextReader }), new Uint8Array([0, 255]));
    const call = h.calls.at(-1);
    assert.equal(call.headers.get('x-tt-csrf'), 'csrf-2');
    assert.equal(JSON.parse(call.body).args.rid, 1);
    assert.equal(notices.length, 1);
    assert.deepEqual(h.errors, []);
});

test('raw byte views, headers and Rust CommandError details preserve their wire contracts', async () => {
    const h = harness();
    await h.ready();
    h.reply(() => Response.json(2));
    const result = await h.transport.invoke('stage_file_chunk', new Uint8Array([9, 1, 2, 8]).subarray(1, 3), {
        headers: { 'file-path': 'opaque', offset: '4' },
    });
    const call = h.calls[1];
    assert.equal(result, 2);
    assert.deepEqual([...call.body], [1, 2]);
    assert.equal(call.headers.get('x-tt-command'), 'stage_file_chunk');
    assert.equal(call.headers.get('x-tt-session'), 'session-1');
    assert.equal(call.headers.get('file-path'), 'opaque');
    assert.equal(call.headers.get('offset'), '4');
    const error = { UpstreamFailure: { status: 429, message: 'Limit exceeded', retryable: true } };
    h.reply(() => Response.json(error, { status: 429 }));
    await assert.rejects(h.transport.invoke('generate'), actual => {
        assert.equal(isWebConnectionError(actual), false);
        assert.deepEqual(actual, error);
        return true;
    });
    await assert.rejects(h.transport.invoke('plugin:fs|open'), /Tauri plugins/);
    await assert.rejects(h.transport.invoke('generate', {}, { headers: { 'x-tt-csrf': 'spoof' } }), /Reserved/);
    await assert.rejects(h.transport.invoke('stage_file_chunk', new Uint8Array([1]), { headers: { 'content-encoding': 'gzip' } }), /Reserved/);
    h.reply(() => Response.json(error, { status: 502 }));
    await assert.rejects(h.transport.invoke('generate'), actual => {
        assert.equal(isWebConnectionError(actual), false, 'structured upstream failures are not proxy outages');
        assert.deepEqual(actual, error);
        return true;
    });
    h.reply(() => new Response('proxy error', { status: 502 }));
    await assert.rejects(h.transport.invoke('generate'), actual => isWebConnectionError(actual));
    h.reply(() => new Response('authentication required', { status: 401 }));
    await assert.rejects(h.transport.invoke('generate'), actual => {
        assert.equal(isWebConnectionError(actual), false);
        assert.match(actual.message, /HTTP 401/);
        return true;
    });
    h.reply(() => new Response('{', { headers: { 'content-type': 'application/json' } }));
    await assert.rejects(h.transport.invoke('generate'), actual => {
        assert.equal(isWebConnectionError(actual), false);
        assert.match(actual.message, /invalid JSON/);
        return true;
    });
    for (const type of ['application/json', 'application/octet-stream']) {
        h.reply(() => new Response(new ReadableStream({
            start(controller) { controller.error(new TypeError('response body disconnected')); },
        }), { headers: { 'content-type': type } }));
        const before = h.calls.length;
        await assert.rejects(h.transport.invoke('generate'), actual => isWebConnectionError(actual));
        assert.equal(h.calls.length, before + 1, 'body loss does not replay the command');
    }
});

test('explicit page exit closes its session with CSRF instead of leaving registrations in the reconnect grace period', async () => {
    const h = harness();
    const stream = await h.ready();
    h.reply(() => new Response(null, { status: 204 }));
    h.emit(h.window, 'pagehide', { persisted: true });
    assert.equal(stream.closed, undefined, 'BFCache retains the page session');
    stream.onerror();
    const waiting = assert.rejects(h.transport.waitUntilConnected(), /page closed/);
    const oldTimer = [...h.timers.values()][0].callback;
    await h.transport.dispose();
    await waiting;
    assert.equal(h.timers.size, 0);
    oldTimer();
    h.emit(h.window, 'online');
    h.emit(h.window, 'pageshow', { persisted: true });
    h.emit(h.document, 'visibilitychange');
    assert.equal(h.streams.length, 1, 'closed transport never restarts');
    const call = h.calls.at(-1);
    assert.equal(call.url, '/__tt/session/close');
    assert.equal(call.keepalive, true);
    assert.equal(call.headers['x-tt-csrf'], 'csrf-1');
    assert.deepEqual(JSON.parse(call.body), { sessionId: 'session-1' });
    assert.equal(stream.closed, true);
    await assert.rejects(h.transport.invoke('save_settings'), /page closed/);
});

test('dispose cancels in-flight session creation and fences its late response while rejecting readiness waiters', async () => {
    const h = harness();
    const release = Promise.withResolvers();
    let request;
    h.sessionReply(init => { request = init; return release.promise; });
    const started = assert.rejects(h.transport.start(), /page closed/);
    const waiting = assert.rejects(h.transport.waitUntilConnected(), /page closed/);
    const channel = h.transport.createChannel(() => {});
    await h.transport.dispose();
    await Promise.all([started, waiting]);
    assert.equal(request.signal.aborted, true);
    assert.throws(() => channel.toJSON(), /no longer active/);
    release.resolve(Response.json({ sessionId: 'late-page', csrfToken: 'late-csrf' }));
    await new Promise(resolve => setImmediate(resolve));
    h.emit(h.window, 'online');
    assert.equal(h.streams.length, 0);
    assert.equal(h.timers.size, 0);
    assert.equal(h.calls.length, 1);
    assert.deepEqual(h.errors, []);
});

test('invalid or skipped event data fails explicitly instead of guessing a recovery', async () => {
    const h = harness();
    const channel = h.transport.createChannel(() => assert.fail('invalid frame must not be dispatched'));
    const { $channel: channelId } = channel.toJSON();
    const stream = await h.ready();
    stream.frame({ type: 'channel', channelId, sequence: 2, payload: 'skipped' }, '0');
    assert.equal(stream.closed, true);
    await assert.rejects(h.transport.invoke('get_settings'), /out-of-order/);
    assert.equal(h.errors.length, 1);
    const other = harness();
    const connected = await other.ready();
    connected.frame({ type: 'heartbeat', sessionId: 'another-page' });
    await assert.rejects(other.transport.waitUntilConnected(), /heartbeat belongs to another session/);
    assert.equal(other.errors.length, 1);
    assert.equal(isWebConnectionError(other.errors[0]), false);
});

test('session reset during request compression rejects the old frame without sending or replaying it', async t => {
    const original = globalThis.CompressionStream;
    const entered = Promise.withResolvers(), release = Promise.withResolvers(), drained = Promise.withResolvers();
    t.after(() => { globalThis.CompressionStream = original; });
    globalThis.CompressionStream = class extends TransformStream {
        constructor() {
            super({
                async transform(bytes, controller) {
                    entered.resolve();
                    await release.promise;
                    controller.enqueue(bytes);
                },
                flush() { drained.resolve(); },
            });
        }
    };
    const h = harness();
    const stream = await h.ready();
    const saving = h.transport.invoke('append_settings_commit_chunk', new Uint8Array(64 * 1024), {
        compressible: true, headers: { 'session-id': 'old-commit', offset: '0' },
    });
    const rejected = assert.rejects(saving, /session was reset/);
    await entered.promise;
    await h.reset(stream);
    await rejected;
    release.resolve();
    await drained.promise;
    // Drain stream consumption and the detached preparation continuation without elapsed-time waits.
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(h.calls.filter(call => call.url === '/__tt/invoke').length, 0);
    h.reply(() => Response.json(true));
    assert.equal(await h.transport.invoke('is_ready'), true);
});

test('settings commits preserve bytes, offsets and confirmed size across HTTP gzip', async t => {
    t.after(installHostIdentity({ platform: 'browser', kind: 'web' }));
    const h = harness();
    await h.ready();
    const text = JSON.stringify({ prompt: '世界书 🍀'.repeat(18000) });
    const frames = [];
    let size = 0, compressedFrames = 0, finished;
    h.reply(init => {
        const command = init.headers.get('x-tt-command');
        if (command) {
            const compressed = init.headers.get('content-encoding') === 'gzip';
            const bytes = compressed ? gunzipSync(init.body) : Buffer.from(init.body);
            if (compressed) {
                compressedFrames++;
                assert.ok(init.body.byteLength < bytes.byteLength);
            }
            assert.equal(init.headers.get('offset'), String(size));
            assert.equal(init.headers.get('session-id'), 'commit');
            assert.equal(init.headers.get('x-tt-session'), 'session-1');
            assert.equal(init.headers.get('x-tt-csrf'), 'csrf-1');
            assert.ok(bytes.byteLength <= 64 * 1024);
            frames.push(bytes);
            size += bytes.byteLength;
            return Response.json(size);
        }
        const request = JSON.parse(init.body);
        if (request.command === 'begin_settings_commit') return Response.json({ sessionId: 'commit', maxFrameBytes: 64 * 1024 });
        if (request.command === 'finish_settings_commit') {
            finished = request.args.expectedSize;
            return Response.json({ result: 'ok' });
        }
        throw new Error(request.command);
    });
    const router = createRouteRegistry();
    registerSettingsRoutes(router, { invokeTransport: h.transport.invoke }, { jsonResponse });
    const response = await router.handle({ method: 'POST', path: '/api/settings/save', body: text, init: {} });
    assert.equal(response.status, 200);
    assert.ok(compressedFrames > 0 && frames.length > 1);
    assert.equal(Buffer.concat(frames).toString(), text);
    assert.equal(finished, Buffer.byteLength(text));
});
