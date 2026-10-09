import test from 'node:test';
import assert from 'node:assert/strict';
import { createWebTransport } from '../src/web/transport.js';

function harness() {
    const calls = [], errors = [], states = [], streams = [], timers = [];
    const streamReady = Promise.withResolvers();
    let reply = () => Response.json(null);
    let created = 0;
    let nextStream;
    class Events {
        constructor(url) { this.url = url; this.nextId = 0; streams.push(this); streamReady.resolve(this); nextStream?.resolve(this); }
        close() { this.closed = true; }
        frame(frame, id = String(this.nextId++)) {
            this.onmessage({ data: JSON.stringify(frame), lastEventId: frame.type === 'ready' || frame.type === 'session-reset' ? '' : id });
        }
    }
    const transport = createWebTransport({
        EventSource: Events,
        onError: error => errors.push(error), onState: state => states.push(state),
        schedule: callback => { timers.push(callback); return timers.length; },
        unschedule: () => {},
        fetch: async (url, init) => {
            calls.push({ url, ...init });
            if (url === '/__tt/session') {
                created++;
                return Response.json({ sessionId: `session-${created}`, csrfToken: `csrf-${created}` });
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
    return { transport, calls, errors, states, streams, timers, reset, streamReady: streamReady.promise, ready,
        reply: handler => { reply = handler; } };
}

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
    const before = h.calls.length;
    h.timers.shift()();
    const resumed = h.streams.at(-1);
    assert.match(resumed.url, /sessionId=session-1&after=0/);
    resumed.frame({ type: 'ready', sessionId: 'session-1' });
    resumed.frame({ type: 'channel', channelId, sequence: 0, payload: 'duplicate' }, '0');
    resumed.frame({ type: 'channel', channelId, sequence: 1, payload: 'second' }, '1');
    resumed.frame({ type: 'event', event: 'change', id: 7, payload: 'yes' }, '2');
    unlisten();
    resumed.frame({ type: 'event', event: 'change', id: 8, payload: 'no' }, '3');
    assert.deepEqual(values, ['first', 'second']);
    assert.deepEqual(events, ['yes']);
    assert.equal(h.calls.length, before, 'never replays the save');
    assert.deepEqual(h.errors, []);
    resumed.frame({ type: 'channel-end', channelId }, '4');
    assert.throws(() => JSON.stringify(channel), /no longer active/);
});

test('failed HTTP commands report unknown outcomes once; later requests are still usable', async () => {
    const h = harness();
    await h.ready();
    h.reply(() => { throw new TypeError('network failed'); });
    await assert.rejects(h.transport.invoke('save_settings'), /outcome may be unknown.*not retried/);
    assert.equal(h.calls.length, 2);
    h.reply(() => Response.json(true));
    assert.equal(await h.transport.invoke('is_ready'), true);
    assert.equal(h.calls.length, 3);
});

test('new server session rejects pending work, changes CSRF and invalidates old resource handles without aliasing new readers', async () => {
    const h = harness();
    const stream = await h.ready();
    h.reply(() => Response.json(1));
    const oldReader = await h.transport.invoke('open_chat_reader', { target: {} });
    const entered = Promise.withResolvers();
    h.reply(init => { entered.resolve(init); return new Promise(() => {}); });
    const unknown = h.transport.invoke('save_settings');
    const rejected = assert.rejects(unknown, /outcomes may be unknown/);
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
    await assert.rejects(h.transport.invoke('generate'), actual => assert.deepEqual(actual, error) === undefined);
    await assert.rejects(h.transport.invoke('plugin:fs|open'), /Tauri plugins/);
    await assert.rejects(h.transport.invoke('generate', {}, { headers: { 'x-tt-csrf': 'spoof' } }), /Reserved/);
    h.reply(() => new Response('proxy error', { status: 502 }));
    await assert.rejects(h.transport.invoke('generate'), /HTTP 502/);
    h.reply(() => new Response('{', { headers: { 'content-type': 'application/json' } }));
    await assert.rejects(h.transport.invoke('generate'), /invalid JSON/);
});

test('explicit page exit closes its session with CSRF instead of leaving registrations in the reconnect grace period', async () => {
    const h = harness();
    const stream = await h.ready();
    h.reply(() => new Response(null, { status: 204 }));
    await h.transport.dispose();
    const call = h.calls.at(-1);
    assert.equal(call.url, '/__tt/session/close');
    assert.equal(call.keepalive, true);
    assert.equal(call.headers['x-tt-csrf'], 'csrf-1');
    assert.deepEqual(JSON.parse(call.body), { sessionId: 'session-1' });
    assert.equal(stream.closed, true);
    await assert.rejects(h.transport.invoke('save_settings'), /page closed/);
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
});
