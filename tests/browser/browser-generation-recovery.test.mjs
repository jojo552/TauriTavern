import assert from 'node:assert/strict';
import test from 'node:test';
import { randomUUID } from 'node:crypto';
import { installHostIdentity } from '../helpers/host-identity.mjs';
import { createBrowserRuntime } from './runtime.mjs';

const FIRST = 'Recovered first half; ';
const LAST = 'the complete tail survives reopening. 完整末尾。';
const TEXT = FIRST + LAST;
const REASONING = 'Earlier thought. Later thought.';
const SIGNATURE = 'opaque-original-signature';
const NATIVE = { gemini: { content: { role: 'model', parts: [{ text: TEXT, thoughtSignature: SIGNATURE }] } } };
const REQUEST_CONTEXT = {
    mainApi: 'openai', chatCompletionSource: 'openrouter', model: 'google/gemini-2.5-pro',
    customApiFormat: 'openai_compat', opencodeApiFormat: 'openai_compat',
};
const TOOL = { id: 'call-original', type: 'function', function: { name: 'recovery_side_effect_probe', arguments: '{}' } };
const MODEL_STARTS = new Set(['start_chat_completion_job', 'start_chat_completion_stream', 'chat_completion', 'generate_chat_completion']);
const plain = value => JSON.parse(JSON.stringify(value));
const records = text => text.split('\n').filter(Boolean).map(line => JSON.parse(line));
const jsonl = values => values.map(value => JSON.stringify(value)).join('\n');
const gate = () => ({ entered: Promise.withResolvers(), release: Promise.withResolvers() });

function baseline(label) {
    return jsonl([
        { user_name: 'User', character_name: 'Review', chat_metadata: { integrity: randomUUID() } },
        { name: 'User', is_user: true, is_system: false, mes: `Earlier floor: ${label}`, send_date: '2026-01-01T12:00:00Z', extra: {} },
        { name: 'Review', is_user: false, is_system: false, mes: 'Historical reply.', send_date: '2026-01-01T12:00:01Z', extra: {} },
        { name: 'User', is_user: true, is_system: false, mes: 'Finish this reply.', send_date: '2026-01-01T12:00:02Z', extra: {} },
    ]);
}

function result(stream, withTool = false) {
    const message = {
        content: TEXT, reasoning: REASONING, native: NATIVE,
        reasoning_details: [{ id: 'reasoning-original', type: 'reasoning.encrypted', data: SIGNATURE }],
        ...(withTool ? { tool_calls: [TOOL] } : {}),
    };
    if (!stream) return { choices: [{ index: 0, message, finish_reason: withTool ? 'tool_calls' : 'stop' }] };
    return [
        { seq: 1, type: 'chunk', data: JSON.stringify({ choices: [{ index: 0, delta: { content: FIRST, reasoning: 'Earlier thought. ' } }] }) },
        { seq: 2, type: 'chunk', data: JSON.stringify({ choices: [{ index: 0, delta: {
            content: LAST, reasoning: 'Later thought.', native: NATIVE, reasoning_details: message.reasoning_details,
            ...(withTool ? { tool_calls: [{ index: 0, ...TOOL }] } : {}),
        } }] }) },
        { seq: 3, type: 'done' },
    ];
}

test('browser generation recovery uses the normal decoder, message finalization and durable chat commit', async (context) => {
    const { window, getModule, load, startHost } = createBrowserRuntime();
    window.happyDOM.settings.timer.maxIntervalIterations = 1;
    window.happyDOM.settings.timer.maxIntervalTime = 0;
    const payloads = new Map(), readers = new Map(), commits = new Map(), jobs = new Map();
    const calls = [], timeline = [], events = [], warnings = [], errors = [];
    let nextId = 1, finishGate, attachGate, ackError, sideEffects = 0;
    let webSession;
    const sourceCreated = Promise.withResolvers();
    const sessionClosed = Promise.withResolvers();
    window.toastr.warning = (...args) => warnings.push(args);
    window.toastr.error = (...args) => errors.push(args);
    const nativeInvoke = window.__TAURI__.core.invoke;

    function openReader(text) {
        const rid = nextId++;
        readers.set(rid, new window.TextEncoder().encode(text));
        return rid;
    }

    // Only the IO boundaries are fake. The real writer still owns UTF-8 framing,
    // immutable snapshots, integrity, commit reason and the save queue.
    async function backend(command, args = {}, options = {}) {
        calls.push({ command, args: command === 'append_chat_commit_chunk' ? undefined : plain(args) });
        if (MODEL_STARTS.has(command)) throw new Error('Recovery must never request another model start');
        switch (command) {
            case 'chat_history_generation_started':
            case 'chat_history_generation_finished':
            case 'prune_agent_chat_persistent_states': return null;
            case 'get_character_chat_integrity': return records(payloads.get(args.fileName))[0].chat_metadata.integrity;
            case 'get_chat_payload_path': return args.fileName;
            case 'plugin:fs|open': return openReader(payloads.get(args.path));
            case 'plugin:fs|fstat': return { size: readers.get(args.rid).length };
            case 'plugin:fs|read': {
                const remaining = readers.get(args.rid), chunk = remaining.subarray(0, args.len);
                const bytes = new window.Uint8Array(args.len + 8);
                bytes.set(chunk);
                new window.DataView(bytes.buffer).setBigUint64(args.len, BigInt(chunk.length));
                readers.set(args.rid, remaining.subarray(chunk.length));
                return bytes;
            }
            case 'open_chat_reader': return openReader(payloads.get(args.target.fileName));
            case 'read_bytes': {
                const remaining = readers.get(args.rid), chunk = remaining.subarray(0, 64);
                readers.set(args.rid, remaining.subarray(chunk.length));
                return new window.Uint8Array(chunk);
            }
            case 'close_resource':
            case 'plugin:resources|close': readers.delete(args.rid); return null;
            case 'begin_chat_commit': {
                const sessionId = String(nextId++);
                commits.set(sessionId, { ...plain(args), frames: [] });
                return { sessionId, maxFrameBytes: 1024 * 1024 };
            }
            case 'append_chat_commit_chunk': {
                const headers = new window.Headers(options.headers);
                const commit = commits.get(headers.get('session-id'));
                const offset = commit.frames.reduce((sum, frame) => sum + frame.length, 0);
                assert.equal(Number(headers.get('offset')), offset);
                commit.frames.push(Buffer.from(args));
                return offset + args.byteLength;
            }
            case 'finish_chat_commit': {
                const commit = commits.get(args.sessionId);
                assert.equal(commit.operation.kind, 'payload');
                const text = Buffer.concat(commit.frames).toString('utf8');
                assert.equal(Buffer.byteLength(text), args.expectedSize);
                const fileName = commit.target.fileName;
                if (finishGate && args.commitReason === 'generationCheckpoint') {
                    finishGate.entered.resolve();
                    await finishGate.release.promise;
                }
                if (!commit.operation.force && records(text)[0].chat_metadata.integrity !== records(payloads.get(fileName))[0].chat_metadata.integrity) {
                    throw { BadRequest: 'integrity' };
                }
                payloads.set(fileName, text);
                commits.delete(args.sessionId);
                timeline.push('commit');
                return null;
            }
            case 'abort_chat_commit': commits.delete(args.sessionId); return null;
            case 'list_chat_completion_jobs': return [...jobs.values()].map(job => ({ jobId: job.jobId, stream: job.stream, presentation: job.presentation }));
            case 'attach_chat_completion_job': {
                const job = jobs.get(args.jobId);
                if (!job) throw { NotFound: 'Generation expired' };
                if (attachGate) {
                    attachGate.entered.resolve();
                    await attachGate.release.promise;
                }
                return { jobId: job.jobId, stream: job.stream, presentation: job.presentation };
            }
            case 'read_chat_completion_job': {
                const job = jobs.get(args.jobId);
                assert.ok(job, 'The original backend job must still exist');
                if (!job.stream) {
                    assert.equal(args.afterSeq, 0);
                    return { stream: false, status: 'done', events: [], result: job.result };
                }
                // Deliver the already-buffered prefix separately from the tail to check
                // that the production cursor/decoder reconstructs one complete reply.
                return args.afterSeq === 0
                    ? { stream: true, status: 'running', events: job.result.slice(0, 1) }
                    : { stream: true, status: 'done', events: job.result.filter(event => event.seq > args.afterSeq) };
            }
            case 'acknowledge_chat_completion_job': {
                const job = jobs.get(args.jobId);
                const saved = records(payloads.get(job.presentation.chatRef.fileName));
                assert.ok(saved.slice(1).some(message => message.extra?.tauritavern?.browserGeneration?.jobId === args.jobId
                    && message.extra.tauritavern.browserGeneration.completed), 'ACK requires a published completed marker');
                timeline.push('ack');
                if (ackError) throw ackError;
                jobs.delete(args.jobId);
                return null;
            }
            case 'cancel_chat_completion_job': throw new Error('Detaching recovery must not cancel the original job');
            default: return nativeInvoke(command, args, options);
        }
    }
    window.__TAURI__.core.invoke = backend;

    class Events {
        constructor(url) {
            assert.equal(new URL(url, window.location.href).pathname, '/__tt/events');
            this.sessionId = new URL(url, window.location.href).searchParams.get('sessionId');
            sourceCreated.resolve(this);
        }
        frame(frame) { this.onmessage({ data: JSON.stringify(frame), lastEventId: '' }); }
        close() { this.closed = true; }
    }
    window.EventSource = Events;

    try {
        // The shared runtime's native bootstrap avoids duplicating a browser boot
        // fixture. From here on bridge calls use actual browser session/HTTP transport.
        await startHost();
        const main = (await load('script.js')).namespace;
        const openai = getModule('scripts/openai.js').namespace;
        const { power_user } = getModule('scripts/power-user.js').namespace;
        const { ToolManager } = getModule('scripts/tool-calling.js').namespace;
        const recoveryJobs = getModule('web/chat-completion-jobs.js').namespace;
        main.reloadMarkdownProcessor();
        getModule('tauri/main/services/chat-surface/chat-virtualization-state.js').namespace
            .initializeChatVirtualization({ chat_virtualization_enabled: false });
        await getModule('scripts/system-messages.js').namespace.initSystemMessages();
        getModule('scripts/tauri/chat/cold-swipes.js').namespace.initializeColdSwipes({ cold_swipes_enabled: false });
        getModule('scripts/extensions.js').namespace.extension_settings.variables = { global: {} };
        getModule('scripts/macros.js').namespace.initMacros();
        main.setAnimationDuration(0);

        window.fetch = async (url, options = {}) => {
            if (url === '/__tt/session') {
                webSession = { sessionId: 'recovery-page', csrfToken: 'recovery-csrf' };
                return window.Response.json(webSession);
            }
            if (url === '/__tt/session/close') {
                sessionClosed.resolve();
                return window.Response.json(null);
            }
            if (url === '/__tt/invoke') {
                const headers = new window.Headers(options.headers);
                assert.equal(headers.get('x-tt-csrf'), webSession.csrfToken);
                const binary = headers.get('content-type') === 'application/octet-stream';
                const request = binary ? { command: headers.get('x-tt-command'), args: options.body, sessionId: headers.get('x-tt-session') } : JSON.parse(options.body);
                assert.equal(request.sessionId, webSession.sessionId);
                try {
                    const value = await backend(request.command, request.args, options);
                    return value instanceof window.Uint8Array
                        ? new window.Response(value, { headers: { 'content-type': 'application/octet-stream' } })
                        : window.Response.json(value ?? null);
                } catch (error) {
                    return window.Response.json(error instanceof Error ? { InternalServerError: error.message } : error, { status: 500 });
                }
            }
            switch (url) {
                case '/api/settings/get': return window.Response.json({ result: 'file not find' });
                case '/api/settings/save': return window.Response.json({ tauritavern_settings_revision: { token: 'saved' } });
                case '/api/characters/merge-attributes':
                case '/api/stats/update': return window.Response.json({ ok: true });
                case '/api/stats/get': return window.Response.json({});
                default: throw new Error(`Unexpected HTTP request: ${url}`);
            }
        };
        await main.getSettings();
        getModule('scripts/tauri/setting/settings-persistence.js').namespace.captureSettingsSaveState({}, { token: 'loaded' });
        main.changeMainAPI('openai');
        installHostIdentity({ platform: 'browser', kind: 'web' }, window);
        const connecting = getModule('tauri-bridge.js').namespace.initializeBridge();
        const source = await sourceCreated.promise;
        source.frame({ type: 'ready', sessionId: source.sessionId });
        await connecting;

        for (const name of ['MESSAGE_RECEIVED', 'CHARACTER_MESSAGE_RENDERED', 'STREAM_TOKEN_RECEIVED', 'GENERATION_STARTED']) {
            main.eventSource.on(main.event_types[name], (...args) => events.push({ name, args: plain(args) }));
        }
        ToolManager.registerFunctionTool({
            name: TOOL.function.name, description: 'Observable recovery side-effect probe',
            parameters: { type: 'object', properties: {} }, action: async () => { sideEffects++; return 'must not run'; },
        });

        const count = command => calls.filter(call => call.command === command).length;
        const emitted = name => events.filter(event => event.name === name).length;
        function clearAudit() { calls.length = timeline.length = events.length = warnings.length = errors.length = 0; }
        function assertNoRestartOrSideEffects() {
            assert.equal(calls.filter(call => MODEL_STARTS.has(call.command)).length, 0);
            assert.equal(emitted('STREAM_TOKEN_RECEIVED'), 0, 'Historical tokens are projection, not new extension events');
            assert.equal(emitted('GENERATION_STARTED'), 0, 'Recovery must not re-enter generation/auto-continue');
            assert.equal(sideEffects, 0);
            assert.equal(count('cancel_chat_completion_job'), 0);
        }
        async function prepare(stream, withTool = false) {
            finishGate = attachGate = ackError = undefined;
            jobs.clear();
            window.localStorage.removeItem('tt:web:pending-completions');
            await main.clearChat({ clearData: true });
            main.setCharacterId(undefined);
            main.setActiveCharacter();
            payloads.set('target-chat', baseline('target'));
            payloads.set('later-chat', baseline('later'));
            main.characters.splice(0, main.characters.length, {
                name: 'Review', avatar: 'Review.png', chat: 'target-chat', shallow: false,
                description: '', personality: '', first_mes: '', scenario: '', mes_example: '', creatorcomment: '',
                create_date: '2026-01-01T12:00:00Z', json_data: '',
                data: { extensions: {}, tags: [], creator_notes: '', creator: '' },
            });
            await main.selectCharacterById(0, { chatFile: 'target-chat' });
            main.setCharacterName('Review');
            Object.assign(openai.oai_settings, {
                chat_completion_source: 'openrouter', openrouter_model: REQUEST_CONTEXT.model,
                show_thoughts: true, function_calling: true, n: 1,
                custom_prompt_post_processing: openai.custom_prompt_post_processing_types.NONE,
            });
            Object.assign(power_user, {
                message_token_count_enabled: false, message_token_rate_enabled: false,
                trim_sentences: false, auto_swipe: true, auto_swipe_minimum_length: 10000,
            });
            Object.assign(power_user.auto_continue, { enabled: true, allow_chat_completions: true, target_length: 10000 });
            const presentation = await recoveryJobs.prepareBrowserChatCompletion('normal', {
                type: 'normal', stream, canMultiSwipe: false, requestContext: REQUEST_CONTEXT,
                isClaudeMessagesRequest: false, jsonSchema: null, showThoughts: true,
            });
            assert.ok(presentation);
            assert.ok(calls.some(call => call.command === 'finish_chat_commit' && call.args.commitReason === 'providerBarrier'),
                'Preparation must publish the provider barrier');
            const job = { jobId: presentation.jobId, stream, presentation: plain(presentation), result: result(stream, withTool) };
            jobs.set(job.jobId, job); // It already ran on the backend; recovery must only attach/read it.
            await main.reloadCurrentChat();
            // Current settings/speaker are deliberately not the original request facts.
            Object.assign(openai.oai_settings, { chat_completion_source: 'claude', claude_model: 'different-current-model', show_thoughts: false });
            main.setCharacterName('Different current speaker');
            clearAudit();
            return job;
        }

        for (const stream of [true, false]) {
            await context.test(`${stream ? 'stream' : 'non-stream'}: complete reply and original provider context publish before ACK, without another generation`, async () => {
                const job = await prepare(stream);
                const before = payloads.get('target-chat');
                const checkpoint = finishGate = gate();
                const recovering = main.recoverBrowserGeneration();
                try {
                    await Promise.race([checkpoint.entered.promise, recovering.then(() => assert.fail('Recovery ended before its checkpoint'))]);
                    assert.equal(payloads.get('target-chat'), before);
                    assert.equal(count('acknowledge_chat_completion_job'), 0);
                    assert.ok(jobs.has(job.jobId));
                } finally {
                    checkpoint.release.resolve();
                    await recovering;
                    finishGate = undefined;
                }
                const saved = records(payloads.get('target-chat'));
                assert.deepEqual(saved.slice(1, -1), records(before).slice(1), 'Historical chat body stays intact');
                const message = saved.at(-1);
                assert.equal(message.mes, TEXT);
                assert.equal(message.name, job.presentation.speaker);
                assert.equal(new Date(message.gen_started).getTime(), job.presentation.startedAt);
                assert.equal(message.extra.reasoning, REASONING);
                assert.equal(message.extra.reasoning_signature, SIGNATURE);
                assert.deepEqual(message.extra.native, NATIVE);
                assert.equal(message.extra.api, REQUEST_CONTEXT.chatCompletionSource);
                assert.equal(message.extra.model, REQUEST_CONTEXT.model);
                assert.deepEqual(message.extra.provider_replay, { ...REQUEST_CONTEXT, text: TEXT });
                assert.deepEqual(message.extra.tauritavern.browserGeneration, { jobId: job.jobId, completed: true });
                assert.deepEqual(timeline, ['commit', 'ack']);
                assert.equal(emitted('MESSAGE_RECEIVED'), 1);
                assert.equal(emitted('CHARACTER_MESSAGE_RENDERED'), 1);
                assert.equal(jobs.has(job.jobId), false);
                assert.equal(window.localStorage.getItem('tt:web:pending-completions'), null);
                if (stream) assert.deepEqual(calls.filter(call => call.command === 'read_chat_completion_job').map(call => call.args.afterSeq), [0, 1]);
                assertNoRestartOrSideEffects();
                assert.deepEqual(errors, []);
            });
        }

        await context.test('a saved completed marker survives lost ACK and reopening only acknowledges, without replay, saving or completion events', async () => {
            const job = await prepare(false);
            ackError = { InternalServerError: 'ACK confirmation unavailable' };
            await main.recoverBrowserGeneration();
            const saved = payloads.get('target-chat');
            assert.ok(jobs.has(job.jobId));
            assert.ok(warnings.length > 0);
            ackError = undefined;
            await main.reloadCurrentChat();
            clearAudit();
            await main.recoverBrowserGeneration();
            assert.equal(payloads.get('target-chat'), saved);
            assert.equal(count('read_chat_completion_job'), 0);
            assert.equal(count('begin_chat_commit'), 0);
            assert.equal(count('acknowledge_chat_completion_job'), 1);
            assert.deepEqual(events, []);
            assert.equal(jobs.has(job.jobId), false);
            assertNoRestartOrSideEffects();
        });

        await context.test('editing an early historical floor refuses recovery and preserves both the edited chat and backend job', async () => {
            const job = await prepare(true);
            main.chat[0].mes = 'Edited first floor, not merely the last message.';
            await main.saveChatConditional();
            await main.reloadCurrentChat();
            const edited = payloads.get('target-chat');
            clearAudit();
            await assert.rejects(main.recoverBrowserGeneration(), /聊天已被编辑|部分回复/);
            assert.equal(payloads.get('target-chat'), edited);
            assert.ok(jobs.has(job.jobId));
            assert.equal(count('read_chat_completion_job'), 0);
            assert.equal(count('begin_chat_commit'), 0);
            assert.equal(count('acknowledge_chat_completion_job'), 0);
            assert.deepEqual(events, []);
            assertNoRestartOrSideEffects();
        });

        await context.test('recovered tool calls are reported and retained, never executed or acknowledged as a completed reply', async () => {
            const job = await prepare(false, true);
            const before = payloads.get('target-chat');
            await assert.rejects(main.recoverBrowserGeneration(), /工具调用/);
            assert.equal(payloads.get('target-chat'), before);
            assert.ok(jobs.has(job.jobId));
            assert.equal(count('begin_chat_commit'), 0);
            assert.equal(count('acknowledge_chat_completion_job'), 0);
            assert.deepEqual(events, []);
            assertNoRestartOrSideEffects();
        });

        await context.test('navigation while attachment is pending cannot apply the old job to the newly selected chat', async () => {
            const job = await prepare(true);
            const target = payloads.get('target-chat'), later = payloads.get('later-chat');
            const attachment = attachGate = gate();
            const recovering = main.recoverBrowserGeneration();
            try {
                await Promise.race([attachment.entered.promise, recovering.then(() => assert.fail('Recovery ended before attachment'))]);
                await main.selectCharacterById(0, { chatFile: 'later-chat' });
                assert.equal(main.getCurrentChatId(), 'later-chat');
            } finally {
                attachment.release.resolve();
                await recovering;
                attachGate = undefined;
            }
            assert.equal(payloads.get('target-chat'), target);
            assert.equal(payloads.get('later-chat'), later);
            assert.deepEqual(Array.from(main.chat, message => message.mes), records(later).slice(1).map(message => message.mes));
            assert.ok(jobs.has(job.jobId));
            assert.equal(count('read_chat_completion_job'), 0);
            assert.equal(count('begin_chat_commit'), 0);
            assert.equal(count('acknowledge_chat_completion_job'), 0);
            assert.equal(emitted('MESSAGE_RECEIVED'), 0);
            assertNoRestartOrSideEffects();
        });
        ToolManager.unregisterFunctionTool(TOOL.function.name);
    } finally {
        finishGate?.release.resolve();
        attachGate?.release.resolve();
        if (webSession) {
            window.dispatchEvent(new window.Event('pagehide'));
            await sessionClosed.promise;
        }
        await window.happyDOM.close();
    }
});
