// @ts-check

import { invoke as hostInvoke, listen as hostListen, onHostSessionReset } from '../../../tauri-bridge.js';

const TAURI_BACKEND_ERROR_EVENT = 'tauritavern-backend-error';
const FRONTEND_BACKEND_ERROR_EVENT = 'tauritavern:backend-error';
const BACKEND_ERROR_QUEUE_KEY = '__TAURITAVERN_BACKEND_ERROR_QUEUE__';
const BACKEND_ERROR_READY_KEY = '__TAURITAVERN_BACKEND_ERROR_CONSUMER_READY__';
const MAX_BACKEND_ERROR_QUEUE_SIZE = 50;

function normalizeBackendErrorPayload(payload) {
    if (typeof payload === 'string') {
        const message = payload.trim();
        return message ? { message } : null;
    }
    if (!payload || typeof payload !== 'object' || Array.isArray(payload)) return null;
    const message = typeof payload.message === 'string' ? payload.message.trim() : '';
    return message ? { message } : null;
}

function publishBackendError(payload) {
    const normalized = normalizeBackendErrorPayload(payload);
    if (!normalized) return;
    if (window[BACKEND_ERROR_READY_KEY]) {
        window.dispatchEvent(new CustomEvent(FRONTEND_BACKEND_ERROR_EVENT, { detail: normalized }));
        return;
    }
    const queuedErrors = Array.isArray(window[BACKEND_ERROR_QUEUE_KEY])
        ? window[BACKEND_ERROR_QUEUE_KEY] : [];
    queuedErrors.push(normalized);
    if (queuedErrors.length > MAX_BACKEND_ERROR_QUEUE_SIZE) {
        queuedErrors.splice(0, queuedErrors.length - MAX_BACKEND_ERROR_QUEUE_SIZE);
    }
    window[BACKEND_ERROR_QUEUE_KEY] = queuedErrors;
}

/**
 * @param {{
 *   invoke?: (command: string) => Promise<unknown>;
 *   listen?: (event: string, handler: (event: { payload: unknown }) => void) => Promise<() => void>;
 *   sessionReset?: (handler: () => Promise<void>) => () => void;
 * }} deps
 */
export async function installBackendErrorBridge({
    invoke = hostInvoke,
    listen = hostListen,
    sessionReset = onHostSessionReset,
} = {}) {
    const stopListening = await listen(TAURI_BACKEND_ERROR_EVENT, (event) => {
        publishBackendError(event?.payload);
    });
    async function declareReady() {
        const pendingErrors = await invoke('backend_error_bridge_ready');
        if (Array.isArray(pendingErrors)) {
            for (const payload of pendingErrors) publishBackendError(payload);
        }
    }
    let stopReset;
    try {
        // Replaced HTTP pages must declare readiness again; a short SSE reconnect
        // keeps this page and does not repeat the declaration or any user operation.
        stopReset = sessionReset(declareReady);
        await declareReady();
    } catch (error) {
        stopReset?.();
        stopListening();
        throw error;
    }
}
