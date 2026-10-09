import { invoke, isWebHost, hostSessionEpoch } from '../../../tauri-bridge.js';
import { createReadableFileStreamService } from '../../../tauri/main/services/files/readable-file-stream-service.js';
import { jsonlStreamToPayload } from './jsonl.js';

const { createByteStream } = createReadableFileStreamService({ invoke });
const pendingSources = new WeakMap();
let currentSource;
let enabled = false;

function currentSourceFor(reference) {
    if (!isWebHost()) return reference.sourceId;
    if (reference.webSessionEpoch !== hostSessionEpoch()) {
        throw new Error('Cold swipe source expired after server reconnect; reopen this chat before loading or saving its swipes');
    }
    return reference.webSourceHandle;
}

export function initializeColdSwipes(settings) { enabled = settings.cold_swipes_enabled === true; }
export function coldSwipesEnabled() { return enabled; }

function closeSource(source) {
    if (!source) return;
    if (isWebHost() && source.epoch !== hostSessionEpoch()) return;
    // Cleanup cannot invalidate a successfully loaded chat.
    void invoke(isWebHost() ? 'close_resource' : 'plugin:resources|close', { rid: source.id }).catch(error => {
        console.warn('Failed to close cold swipe source', error);
    });
}

export function releaseCurrentSwipeSource() {
    const previous = currentSource;
    currentSource = undefined;
    closeSource(previous);
}

export function acceptColdChatPayload(payload) {
    const next = pendingSources.get(payload);
    pendingSources.delete(payload);
    const previous = currentSource;
    currentSource = next;
    if (previous !== next) closeSource(previous);
}

export function discardColdChatPayload(payload) {
    const source = pendingSources.get(payload);
    pendingSources.delete(payload);
    closeSource(source);
}

export async function loadColdChatPayload(target, allowNotFound) {
    const opened = await invoke('open_cold_chat', { target, allowNotFound });
    if (!opened) return [];
    const source = { id: opened.sourceId, epoch: hostSessionEpoch() };
    try {
        const payload = await jsonlStreamToPayload(createByteStream(opened.readerId));
        let hasColdReferences = false;
        for (const [index, record] of payload.entries()) {
            if (index > 0 && record.tt_swipe_cold) {
                // sourceId remains the server id for commit reintegration. Provenance must
                // survive structuredClone (bookmarks/message copies) but can never alias a
                // replacement page's resource. The private cold marker is consumed on save.
                if (isWebHost()) {
                    record.tt_swipe_cold.webSessionEpoch = source.epoch;
                    record.tt_swipe_cold.webSourceHandle = source.id;
                }
                hasColdReferences = true;
            }
        }
        if (hasColdReferences) pendingSources.set(payload, source);
        else closeSource(source);
        return payload;
    } catch (error) {
        closeSource(source);
        throw error;
    }
}

export async function readColdSwipeRecord(reference) {
    const sourceId = currentSourceFor(reference);
    const rid = await invoke('open_cold_swipe_record', { sourceId, record: reference.record });
    const records = await jsonlStreamToPayload(createByteStream(rid), { hasHeader: false });
    if (records.length !== 1) throw new Error('Cold swipe source did not return one message');
    return records[0];
}

/** Fill unloaded slots in place; live values and appended slots remain authoritative. */
export async function hydrateMessageSwipes(message) {
    if (!message?.tt_swipe_cold) return;
    const original = await readColdSwipeRecord(message.tt_swipe_cold);
    const current = message.tt_swipe_cold;
    if (!current) return;
    const keys = ['swipes', 'swipe_info'];
    for (const key of keys) {
        if (!Array.isArray(message[key]) || message[key].length < original[key].length) {
            throw new Error('Cold swipe array shortened; load swipes before deleting slots');
        }
    }
    for (const key of keys) {
        for (let i = 0; i < original[key].length; i++) message[key][i] ??= original[key][i];
    }
    delete message.tt_swipe_cold;
}

/** The capture runs synchronously alongside JSON serialization; one commit has one opened source. */
export function coldSourceForPayload(payload) {
    let sourceId;
    for (let index = 1; index < payload.length; index++) {
        const reference = payload[index].tt_swipe_cold;
        if (!reference) continue;
        if (!Number.isInteger(reference.sourceId) || reference.sourceId < 0) throw new Error('Cold swipe message has no source');
        const currentId = currentSourceFor(reference);
        if (sourceId !== undefined && sourceId !== currentId) throw new Error('Cannot save a chat with mixed cold swipe sources');
        sourceId = currentId;
    }
    return sourceId;
}
