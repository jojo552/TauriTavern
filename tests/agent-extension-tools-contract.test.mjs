import test from 'node:test';
import assert from 'node:assert/strict';
import { createAgentToolsApi } from '../src/tauri/main/api/agent-tools.js';

const definition = {
    extensionId: 'third-party/example', name: 'inspect', description: 'Inspect application state.',
    inputSchema: { type: 'object' }, contexts: ['chat', 'session'],
};

function harness() {
    const channels = [], replies = [], registrations = [];
    let reset, register = async () => {};
    const api = createAgentToolsApi({
        sessionReset(handler) { reset = handler; return () => {}; },
        channelFactory(onmessage) { const channel = { onmessage }; channels.push(channel); return channel; },
        async safeInvoke(command, args) {
            if (command === 'register_agent_extension_tool') {
                registrations.push(structuredClone(args.definition));
                await register();
            }
            if (command === 'resolve_agent_extension_tool_call') {
                replies.push(JSON.parse(JSON.stringify(args.dto)));
                return true;
            }
        },
    });
    const call = (requestId, args = {}) => ({
        type: 'call', requestId,
        call: { toolId: 'extension/third-party/example:inspect', runId: 'run-1', invocationId: 'inv-1', callId: 'call-1', target: { kind: 'session', sessionId: 'session-1' }, arguments: args },
    });
    return { api, channels, replies, call, registrations, reset: () => reset(), registrationReply: reply => { register = reply; } };
}

test('registered extension callbacks await async closures before returning JSON results', async () => {
    const { api, channels, replies, call } = harness();
    let multiplier = 2;
    const ready = Promise.withResolvers();
    await api.register(definition, async (args) => { await ready.promise; return { answer: args.value * multiplier }; });
    const completed = channels[0].onmessage(call('request-1', { value: 7 }));
    assert.deepEqual(replies, []);
    multiplier = 3;
    ready.resolve();
    await completed;
    assert.deepEqual(replies, [{ requestId: 'request-1', result: { kind: 'value', value: { answer: 21 } } }]);
});

test('undefined returns null while callback and JSON serialization failures return tool errors', async () => {
    const { api, channels, replies, call } = harness();
    const cyclic = {}; cyclic.self = cyclic;
    await api.register(definition, ({ mode }) => {
        if (mode === 'throw') throw new Error('tool failed');
        if (mode === 'cyclic') return cyclic;
    });
    for (const mode of ['void', 'throw', 'cyclic']) await channels[0].onmessage(call(mode, { mode }));
    assert.deepEqual(replies[0].result, { kind: 'value', value: null });
    assert.deepEqual(replies[1].result, { kind: 'error', message: 'tool failed' });
    assert.equal(replies[2].result.kind, 'error');
});

test('cancellation aborts only its request and suppresses late completion', async () => {
    const { api, channels, replies, call } = harness();
    const blocked = Promise.withResolvers(), contexts = [];
    await api.register(definition, async (_, context) => { contexts.push(context); await blocked.promise; return 'complete'; });
    const first = channels[0].onmessage(call('first'));
    const second = channels[0].onmessage(call('second'));
    channels[0].onmessage({ type: 'cancel', requestId: 'first' });
    assert.equal(contexts[0].signal.aborted, true);
    assert.equal(contexts[1].signal.aborted, false);
    blocked.resolve();
    await Promise.all([first, second]);
    assert.deepEqual(replies, [{ requestId: 'second', result: { kind: 'value', value: 'complete' } }]);
});

test('session reset cancels callbacks even when initial registration has not been acknowledged', async () => {
    const h = harness();
    const entered = Promise.withResolvers(), acknowledged = Promise.withResolvers(), finished = Promise.withResolvers();
    h.registrationReply(() => { entered.resolve(); return acknowledged.promise; });
    let signal;
    const registering = h.api.register(definition, async (_, context) => { signal = context.signal; await finished.promise; return 'old'; });
    const rejected = assert.rejects(registering, /session reset/);
    await entered.promise;
    const callback = h.channels[0].onmessage(h.call('attaching'));
    await h.reset();
    assert.equal(signal.aborted, true);
    finished.resolve();
    await callback;
    acknowledged.reject(new Error('session reset'));
    await rejected;
    assert.deepEqual(h.replies, []);
    assert.equal(h.registrations.length, 1, 'an unknown registration is never automatically retried');
});

test('new page session rebuilds confirmed registrations and enabled state, never old pending tool execution', async () => {
    const h = harness();
    const blocked = Promise.withResolvers();
    let context;
    await h.api.register(definition, async (args, supplied) => { context = supplied; await blocked.promise; return args; });
    await h.api.setEnabled('extension/third-party/example:inspect', false);
    const oldCall = h.channels[0].onmessage(h.call('old', { answer: 42 }));
    await h.reset();
    assert.equal(context.signal.aborted, true);
    assert.equal(h.registrations.length, 2);
    assert.equal(h.registrations[1].enabled, false);
    assert.equal(h.channels.length, 2);
    blocked.resolve();
    await oldCall;
    assert.deepEqual(h.replies, [], 'no stale reply reaches the replacement page');
    await h.channels[1].onmessage(h.call('new', { answer: 7 }));
    assert.equal(h.replies[0].requestId, 'new');
});
