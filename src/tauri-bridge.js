// Common host bridge for native Tauri, Studio embeddings and the standalone HTTP host.
import { isWebHost } from './scripts/util/host-identity.js';
import { createWebHostApi } from './web/host.js';

export { isWebHost };
export { WebConnectionError, isWebConnectionError, WebSessionResetError, isWebSessionResetError } from './web/transport.js';

let webHostApi;
function getWebHostApi() { return webHostApi ??= createWebHostApi(); }
function getWebTransport() { return getWebHostApi().transport; }

// Resolve lazily: native injection may complete after early modules have been evaluated.
// Studio installs the upstream host ABI; HTTP never exposes it as a global Tauri runtime.
function getHostApi() {
    if (isWebHost()) return getWebHostApi();
    return typeof window === 'undefined' ? null : window.__TAURI__ || null;
}

// Recovery controls belong only to the HTTP page connection. They never replay invoke.
export function waitUntilWebConnected(options) { return getWebTransport().waitUntilConnected(options); }
export function recoverWebConnection() { return getWebTransport().recoverConnection(); }
export function signOutWebHost() { return getWebTransport().signOut(); }
export function hostSessionEpoch() { return isWebHost() ? getWebTransport().sessionEpoch() : 0; }
export function onHostSessionReset(handler) {
    return isWebHost() ? getWebTransport().onSessionReset(handler) : () => {};
}
export async function getCsrfToken() {
    return isWebHost() ? getWebTransport().csrfToken() : 'tauri-dummy-token';
}

function isPlainObject(value) {
    if (Object.prototype.toString.call(value) !== '[object Object]') return false;
    const prototype = Object.getPrototypeOf(value);
    return prototype === null || prototype === Object.prototype;
}

function withTauriArgumentAliases(args) {
    if (!isPlainObject(args)) return args;
    const aliased = { ...args };
    for (const [key, value] of Object.entries(args)) {
        if (!key.includes('_')) continue;
        const camelCaseKey = key.replace(/_+([a-zA-Z0-9])/g, (_, char) => char.toUpperCase());
        if (!Object.prototype.hasOwnProperty.call(aliased, camelCaseKey)) aliased[camelCaseKey] = value;
    }
    return aliased;
}

export const invoke = (...args) => {
    const fn = getHostApi()?.core?.invoke;
    if (typeof fn !== 'function') throw new Error('Tauri invoke is unavailable');
    if (args.length === 2 && isPlainObject(args[1])) return fn(args[0], withTauriArgumentAliases(args[1]));
    return fn(...args);
};

function getHostSafeInvoke() {
    if (typeof window === 'undefined') return null;
    const fn = window.__TAURITAVERN__?.invoke?.safeInvoke;
    return typeof fn === 'function' ? fn : null;
}

async function invokeWithHostNormalization(command, args) {
    const safeInvoke = getHostSafeInvoke();
    if (safeInvoke) return safeInvoke(command, args);
    return args === undefined ? invoke(command) : invoke(command, args);
}

export const listen = (...args) => {
    const fn = getHostApi()?.event?.listen;
    if (typeof fn !== 'function') throw new Error('Tauri listen is unavailable');
    return fn(...args);
};

export function createChannel(onmessage) {
    const Channel = getHostApi()?.core?.Channel;
    if (typeof Channel !== 'function') throw new Error('Tauri Channel is unavailable');
    return new Channel(onmessage);
}

export function getCurrentWindow() {
    const currentWindow = getHostApi()?.window?.getCurrentWindow;
    if (typeof currentWindow !== 'function') throw new Error('Host window API is unavailable');
    return currentWindow();
}

// An optional scanner can be absent; querying it must not claim the capability exists.
export function getBarcodeScanner() { return getHostApi()?.barcodeScanner; }

export const convertFileSrc = (path, protocol = 'asset') => {
    const fn = getHostApi()?.core?.convertFileSrc;
    if (typeof fn !== 'function') throw new Error('Tauri convertFileSrc is unavailable');
    return fn(path, protocol);
};

export async function getClientVersion() { return invoke('get_client_version'); }
export async function checkForUpdate(channel) { return invokeWithHostNormalization('check_for_update', { channel }); }
export async function getTauriTavernSettings() { return invoke('get_tauritavern_settings'); }
export async function getChatBackupStorageStats() { return invoke('get_chat_backup_storage_stats'); }

export async function updateTauriTavernSettings(dto) {
    if (!isPlainObject(dto)) throw new Error('Invalid TauriTavern settings DTO');
    return invokeWithHostNormalization('update_tauritavern_settings', { dto });
}

export async function getRuntimePaths() { return invoke('get_runtime_paths'); }

export async function setDataRoot(dataRoot) {
    if (typeof dataRoot !== 'string' || dataRoot.trim() === '') throw new Error('Invalid data root path');
    return invokeWithHostNormalization('set_data_root', { data_root: dataRoot });
}

export async function openDialog(options = {}) {
    if (!isPlainObject(options)) throw new Error('Invalid dialog options: expected an object');
    Object.freeze(options);
    return invokeWithHostNormalization('plugin:dialog|open', { options });
}

export function writeClipboardText(text) {
    const host = getHostApi();
    if (host?.writeClipboardText) return host.writeClipboardText(text);
    return invoke('plugin:clipboard-manager|write_text', { text });
}

function normalizeExternalUrl(url) {
    const value = String(url instanceof URL ? url.href : url ?? '').trim();
    if (!value) throw new Error('External URL is required');
    try { return new URL(value, window.location.href).toString(); }
    catch { throw new Error(`Invalid external URL: ${value}`); }
}

export async function openExternalUrl(url, openWith) {
    const href = normalizeExternalUrl(url);
    const host = getHostApi();
    if (host?.openExternalUrl) return host.openExternalUrl(href);
    return invoke('plugin:opener|open_url', { url: href, with: openWith });
}
