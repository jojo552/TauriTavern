import { createFileStagingService } from '../tauri/main/services/files/file-staging-service.js';
import { createBrowserFileTransfer } from '../web/file-transfer.js';
import { isWebHost } from '../tauri-bridge.js';

/**
 * Native delivery confirms a target write or system transfer; browser delivery
 * only confirms handoff because saving and cancellation cannot be observed.
 * @typedef {{ delivered: boolean } | { status: 'handed-off' }} FileDeliveryResult
 */

/** @returns {Promise<FileDeliveryResult>} */
export async function deliverBlob(blob, fileName) {
    const payload = blob instanceof Blob ? blob : new Blob([blob ?? '']);
    const invoke = window.__TAURI__?.core?.invoke;
    if (typeof invoke !== 'function') {
        return createBrowserFileTransfer().downloadBlob(payload, fileName || 'download.bin');
    }

    const { safeInvoke } = window.__TAURITAVERN__.invoke;
    const staging = createFileStagingService({ safeInvoke, invoke });
    const path = await staging.stageBlob(payload, { kind: 'export', preferredName: fileName });
    return safeInvoke('deliver_staged_file', { path, fileName });
}

/**
 * Stages remote bytes in the host, then names the file using the response type.
 * @param {string} url
 * @param {(mimeType: string) => string} fileNameFor
 * @returns {Promise<FileDeliveryResult>}
 */
export async function deliverRemoteFile(url, fileNameFor) {
    if (isWebHost()) {
        const target = new URL(url, window.location.href);
        if (target.origin !== window.location.origin) {
            throw new Error('Browser remote export requires a same-origin URL; external export requires a host download artifact');
        }
        const response = await fetch(target.href, { credentials: 'same-origin' });
        if (!response.ok) throw new Error(`Remote export failed: HTTP ${response.status}`);
        return createBrowserFileTransfer().downloadBlob(await response.blob(), fileNameFor(response.headers.get('content-type') ?? ''));
    }
    const { safeInvoke } = window.__TAURITAVERN__.invoke;
    const { path, mimeType } = await createFileStagingService({ safeInvoke }).stageUrl(url);
    return safeInvoke('deliver_staged_file', { path, fileName: fileNameFor(mimeType) });
}
