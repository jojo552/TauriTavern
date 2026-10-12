import {
    invoke, isWebHost, isWebConnectionError, isWebSessionResetError,
    recoverWebConnection, waitUntilWebConnected,
} from '../tauri-bridge.js';
import { CHAT_COMMIT_REASON } from '../scripts/chat-payload-transport.js';

const PENDING_KEY = 'tt:web:pending-completions';
const MAX_PENDING = 16;

export class BrowserGenerationDetachedError extends Error {}

export class BrowserGenerationRecoveryError extends Error {
    constructor(jobId, message) {
        super(message);
        this.jobId = jobId;
    }
}

function pendingJobs() {
    const raw = localStorage.getItem(PENDING_KEY);
    if (!raw) return [];
    const jobs = JSON.parse(raw);
    if (!Array.isArray(jobs) || jobs.some(job => typeof job?.jobId !== 'string' || !job.chatRef)) {
        throw new Error('Invalid pending browser generations');
    }
    return jobs;
}

function forgetJob(jobId) {
    const jobs = pendingJobs().filter(job => job.jobId !== jobId);
    if (jobs.length) localStorage.setItem(PENDING_KEY, JSON.stringify(jobs));
    else localStorage.removeItem(PENDING_KEY);
}

export function sameGenerationChat(a, b) {
    if (!a || !b || a.kind !== b.kind) return false;
    return a.kind === 'character'
        ? a.characterId === b.characterId && a.fileName === b.fileName
        : a.kind === 'group' && a.chatId === b.chatId;
}

// Hash logical message content, not a cold reader's page-lifetime resource handles.
// All other fields remain significant: an edited earlier message must block restoration too.
function canonical(value, coldReference = false) {
    if (Array.isArray(value)) return value.map(item => canonical(item));
    if (!value || typeof value !== 'object') return value;
    if (value instanceof Date) return value.toJSON();
    return Object.fromEntries(Object.keys(value).sort()
        .filter(key => value[key] !== undefined
            && !(coldReference && ['sourceId', 'webSourceHandle', 'webSessionEpoch'].includes(key)))
        .map(key => [key, canonical(value[key], key === 'tt_swipe_cold')]));
}

export async function generationChatDigest(messages) {
    const bytes = new TextEncoder().encode(JSON.stringify(canonical(messages)));
    const digest = await crypto.subtle.digest('SHA-256', bytes);
    return Array.from(new Uint8Array(digest), byte => byte.toString(16).padStart(2, '0')).join('');
}

/** Capture only confirmed presentation facts, never the model prompt or credentials. */
export async function prepareBrowserChatCompletion(type, parser) {
    if (!isWebHost() || !['normal', 'regenerate'].includes(type) || parser.jsonSchema) return null;
    const script = await import('../script.js');
    const context = window.SillyTavern.getContext();
    // A temporary/neutral chat has no durable locator. Its ordinary generation is still valid.
    if (!script.getCurrentChatId() || (!context.groupId && script.this_chid === undefined)) return null;
    const chatRef = window.__TAURITAVERN__.api.chat.current.ref();
    await script.saveChatConditional(CHAT_COMMIT_REASON.PROVIDER_BARRIER);
    if (!sameGenerationChat(chatRef, window.__TAURITAVERN__.api.chat.current.ref())) {
        throw new Error('The chat changed before the generation could start');
    }
    const messages = structuredClone(script.chat);
    const baseDigest = await generationChatDigest(messages);
    if (!sameGenerationChat(chatRef, window.__TAURITAVERN__.api.chat.current.ref())
        || JSON.stringify(script.chat) !== JSON.stringify(messages)) {
        throw new Error('The chat changed while preparing generation recovery');
    }
    const jobId = crypto.randomUUID();
    const presentation = {
        jobId, chatRef, type, baseLength: messages.length, baseDigest,
        ...script.captureBrowserGenerationPresentation(),
        startedAt: Date.now(), parser: { ...parser, jsonSchema: null },
    };
    const jobs = pendingJobs();
    if (jobs.length >= MAX_PENDING) throw new Error('Too many unconfirmed browser generations; reopen their chats first');
    // Discovery only; the server owns existence/results. Record before start so a lost
    // confirmation can be queried by ID, never by repeating the model request.
    localStorage.setItem(PENDING_KEY, JSON.stringify([...jobs, { jobId, chatRef }]));
    return presentation;
}

export function markBrowserChatCompletionMessage(message, jobId) {
    if (!jobId || !message) return;
    message.extra ??= {};
    message.extra.tauritavern ??= {};
    message.extra.tauritavern.browserGeneration = { jobId, completed: true };
}

export async function acknowledgeBrowserChatCompletion(jobId) {
    if (!jobId) return;
    try {
        await invoke('acknowledge_chat_completion_job', { jobId });
        forgetJob(jobId);
    } catch (error) {
        if (hasCommandError(error, 'NotFound')) {
            forgetJob(jobId); // Already acknowledged/expired; the saved chat remains authoritative.
            return;
        }
        // Never repeat a write whose confirmation was lost. Reopening checks the saved marker.
        console.warn('Generation saved, but server acknowledgement was not confirmed', error);
        globalThis.toastr?.warning?.('回复已保存，但任务确认未完成；重新打开聊天后会核对，不会重新生成。');
    }
}

function hasCommandError(error, name) {
    return Boolean(error && typeof error === 'object' && Object.hasOwn(error, name));
}

/** Only explicitly safe queries/declarations enter this loop; never start, save or cancel. */
export async function queryBrowserChatCompletion(command, args = {}, signal) {
    if (!['read_chat_completion_job', 'list_chat_completion_jobs', 'attach_chat_completion_job'].includes(command)) {
        throw new Error(`Not a recoverable generation query: ${command}`);
    }
    let reattach = false;
    for (;;) {
        signal?.throwIfAborted();
        try {
            if (reattach && command === 'read_chat_completion_job') {
                await invoke('attach_chat_completion_job', { jobId: args.jobId }, { signal });
                reattach = false;
            }
            return await invoke(command, args, { signal });
        } catch (error) {
            if (!isWebConnectionError(error) && !isWebSessionResetError(error)) throw error;
            reattach = command === 'read_chat_completion_job';
            if (isWebConnectionError(error)) recoverWebConnection();
            await waitUntilWebConnected({ signal });
        }
    }
}

export async function readBrowserChatCompletion(jobId, afterSeq, signal) {
    const value = await queryBrowserChatCompletion('read_chat_completion_job', { jobId, afterSeq }, signal);
    if (value.stream && value.status === 'error' && value.error && value.events?.length === 0) throw value.error;
    return value;
}

export async function cancelBrowserChatCompletion(jobId) {
    await invoke('cancel_chat_completion_job', { jobId });
    forgetJob(jobId);
}

export async function waitBrowserChatCompletionResult(jobId, signal) {
    const result = await readBrowserChatCompletion(jobId, 0, signal);
    if (result.status === 'error') throw result.error;
    if (result.status === 'cancelled') throw new DOMException('Generation cancelled', 'AbortError');
    if (result.stream || result.status !== 'done' || !Object.hasOwn(result, 'result')) {
        throw new Error('Invalid non-streaming browser generation result');
    }
    return result.result;
}

export async function assertBrowserChatCompletionBaseline(presentation, chatRef, messages) {
    if (!sameGenerationChat(presentation.chatRef, chatRef) || messages.length !== presentation.baseLength
        || await generationChatDigest(messages) !== presentation.baseDigest) {
        throw new Error('旧生成仍保留，但聊天已被编辑或保存了部分回复；为避免覆盖，不能自动恢复。');
    }
}

/** Resolve a stored result only; application to chat is owned by the normal completion pipeline. */
export async function findBrowserChatCompletionRecovery(chatRef, messages, signal) {
    const jobs = await queryBrowserChatCompletion('list_chat_completion_jobs', {}, signal);
    if (!Array.isArray(jobs)) throw new Error('Invalid browser generation list');
    const related = jobs.filter(job => sameGenerationChat(job.presentation?.chatRef, chatRef));
    // A server restart destroys in-memory runs. A local index makes that loss explicit.
    for (const pending of pendingJobs().filter(job => sameGenerationChat(job.chatRef, chatRef))) {
        if (jobs.some(job => job.jobId === pending.jobId)) continue;
        try {
            const job = await queryBrowserChatCompletion('attach_chat_completion_job', { jobId: pending.jobId }, signal);
            related.push(job);
        } catch (error) {
            if (hasCommandError(error, 'NotFound')) {
                forgetJob(pending.jobId);
                globalThis.toastr?.warning?.('旧生成已失效（服务器重启或任务到期）；不会自动重新请求模型。');
            } else if (!hasCommandError(error, 'Conflict')) throw error;
        }
    }
    for (const job of related) {
        const completed = messages.some(message => {
            const marker = message.extra?.tauritavern?.browserGeneration;
            return marker?.jobId === job.jobId && marker.completed === true;
        });
        if (completed) {
            await queryBrowserChatCompletion('attach_chat_completion_job', { jobId: job.jobId }, signal);
            await acknowledgeBrowserChatCompletion(job.jobId);
            continue;
        }
        try { await assertBrowserChatCompletionBaseline(job.presentation, chatRef, messages); }
        catch (error) { throw new BrowserGenerationRecoveryError(job.jobId, error.message); }
        return queryBrowserChatCompletion('attach_chat_completion_job', { jobId: job.jobId }, signal);
    }
    return null;
}
