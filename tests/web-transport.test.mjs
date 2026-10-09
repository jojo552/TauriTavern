import test from 'node:test';
import assert from 'node:assert/strict';
import { createWebTransport } from '../src/web/transport.js';

function harness() {
    const calls = [];
    const errors = [];
    const streamReady = Promise.withResolvers();
    let stream;
    let reply = () => Response.json(null);
    class Events {
        constructor(url) { this.url = url; stream = this; streamReady.resolve(this); }
        close() { this.closed = true; }
        frame(frame) { this.onmessage({ data: JSON.stringify(frame) }); }
    }
    const transport = createWebTransport({
        EventSource: Events,
        onError: error => errors.push(error),
        fetch: async (url, init) => {
            calls.push({ url, ...init });
            if (url === '/__tt/session') return Response.json({ sessionId: 'session-1', csrfToken: 'csrf-1' });
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
    return { transport, calls, errors, streamReady: streamReady.promise, ready,
        get stream() { return stream; }, reply: handler => { reply = handler; } };
}

test('invoke waits for session registration, keeps DTOs intact and sends CSRF with same-origin credentials', async () => {
    const h = harness();
    h.reply(() => Response.json({ name: 'Alice', data: [1, 2] }));
    const result = h.transport.invoke('get_character', { character_id: 'Alice', dto: { camelCase: 3 } });
    const stream = await h.streamReady;
    assert.equal(h.calls.length, 1, 'no invoke before the host registers the stream');
    stream.frame({ type: 'ready', sessionId: 'session-1' });
    assert.deepEqual(await result, { name: 'Alice', data: [1, 2] });
    const call = h.calls[1];
    assert.deepEqual(JSON.parse(call.body), { sessionId: 'session-1', command: 'get_character', args: { character_id: 'Alice', dto: { camelCase: 3 } } });
    assert.equal(call.headers.get('x-tt-csrf'), 'csrf-1');
    assert.equal(call.credentials, 'same-origin');
});

test('Channel is live before invoke and remains live after registration completes; event unlisten is isolated', async () => {
    const h = harness();
    const values = [];
    const events = [];
    const channel = h.transport.createChannel(value => values.push(value));
    const stop = h.transport.listen('tauritavern-backend-error', event => events.push(event));
    const stream = await h.ready();
    const unlisten = await stop;
    await h.transport.invoke('register_agent_extension_tool', { definition: { name: 'inspect' }, channel });
    const channelId = JSON.parse(h.calls[1].body).args.channel.$channel;
    stream.frame({ type: 'channel', channelId, sequence: 0, payload: { requestId: 'first' } });
    stream.frame({ type: 'event', event: 'tauritavern-backend-error', id: 10, payload: { message: 'failed' } });
    unlisten();
    stream.frame({ type: 'event', event: 'tauritavern-backend-error', id: 11, payload: { message: 'later' } });
    stream.frame({ type: 'channel', channelId, sequence: 1, payload: { requestId: 'second' } });
    assert.deepEqual(values, [{ requestId: 'first' }, { requestId: 'second' }]);
    assert.deepEqual(events.map(event => event.payload), [{ message: 'failed' }]);
    stream.frame({ type: 'channel-end', channelId });
    assert.throws(() => JSON.stringify(channel), /no longer active/);
});

test('raw upload views preserve byte boundaries and headers; raw replies become Uint8Array', async () => {
    const h = harness();
    await h.ready();
    h.reply(() => new Response(new Uint8Array([0, 255]), { headers: { 'content-type': 'application/octet-stream' } }));
    const result = await h.transport.invoke('stage_file_chunk', new Uint8Array([9, 1, 2, 8]).subarray(1, 3), {
        headers: { 'file-path': 'opaque', offset: '4' },
    });
    const call = h.calls[1];
    assert.deepEqual([...call.body], [1, 2]);
    assert.equal(call.headers.get('x-tt-command'), 'stage_file_chunk');
    assert.equal(call.headers.get('x-tt-session'), 'session-1');
    assert.equal(call.headers.get('file-path'), 'opaque');
    assert.equal(call.headers.get('offset'), '4');
    assert.deepEqual(result, new Uint8Array([0, 255]));
});

test('CommandError details are preserved and unknown/plugin commands, spoofed headers and invalid responses fail explicitly', async () => {
    const h = harness();
    await h.ready();
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

test('disconnect closes the stream, fails pending work, and never retries an unknown command outcome', async () => {
    const h = harness();
    const stream = await h.ready();
    const entered = Promise.withResolvers();
    h.reply(init => { entered.resolve(init); return new Promise(() => {}); });
    const pending = h.transport.invoke('write_settings', { data: 'test-only' });
    const call = await entered.promise;
    stream.onerror();
    await assert.rejects(pending, /outcomes may be unknown/);
    await assert.rejects(h.transport.invoke('write_settings'), /outcomes may be unknown/);
    assert.equal(stream.closed, true);
    assert.equal(call.signal.aborted, true);
    assert.equal(h.calls.length, 2);
    assert.equal(h.errors.length, 1);
});

test('out-of-order Channel data fails the session rather than dropping or guessing a frame', async () => {
    const h = harness();
    const channel = h.transport.createChannel(() => assert.fail('invalid frame must not be dispatched'));
    const { $channel: channelId } = channel.toJSON();
    const stream = await h.ready();
    stream.frame({ type: 'channel', channelId, sequence: 2, payload: 'skipped' });
    assert.equal(stream.closed, true);
    await assert.rejects(h.transport.invoke('get_settings'), /out-of-order/);
    assert.equal(h.errors.length, 1);
});
