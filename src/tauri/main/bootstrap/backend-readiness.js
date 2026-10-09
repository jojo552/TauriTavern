// @ts-check

import { invoke, isWebHost } from '../../../tauri-bridge.js';

export async function waitForBackendReady() {
    if (!isWebHost() && typeof window.__TAURI__?.core?.invoke !== 'function') {
        throw new Error('Tauri invoke is unavailable for backend readiness');
    }

    await invoke('wait_for_backend_ready');
}
