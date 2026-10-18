// Browser file selection and delivery. No server path or Tauri plugin is involved.
const ACCEPT_BY_KIND = Object.freeze({
    'character-card': '.json,.png',
    'skill-archive': '.zip,.ttskill',
    'data-archive': '.zip,.tar,.gz,.tgz',
});

/**
 * Keep DOM access at the host boundary so callers and tests use the same behavior.
 * Downloads report browser handoff, not successful disk writes (which browsers cannot observe).
 */
export function createBrowserFileTransfer({
    document = globalThis.document,
    urlApi = globalThis.URL,
    schedule = globalThis.setTimeout,
} = {}) {
    function requireBody() {
        if (!document?.body) throw new Error('Browser file transfer requires a document body');
        return document.body;
    }

    /**
     * Must be called during a user gesture. Cancellation returns null; failures reject.
     * @param {{ kind?: keyof typeof ACCEPT_BY_KIND; multiple?: boolean; signal?: AbortSignal }} [options]
     */
    function pickFiles({ kind, multiple = false, signal } = {}) {
        if (!Object.hasOwn(ACCEPT_BY_KIND, kind)) {
            throw new TypeError(`Unsupported file selection kind: ${kind}`);
        }
        if (typeof multiple !== 'boolean') throw new TypeError('multiple must be a boolean');
        const body = requireBody();
        signal?.throwIfAborted();
        const input = document.createElement('input');
        input.type = 'file';
        input.accept = ACCEPT_BY_KIND[kind];
        input.multiple = multiple;
        input.hidden = true;

        return new Promise((resolve, reject) => {
            function cleanup() {
                input.removeEventListener('change', onChange);
                input.removeEventListener('cancel', onCancel);
                signal?.removeEventListener('abort', onAbort);
                input.remove();
            }
            function onChange() {
                const files = Array.from(input.files ?? []);
                cleanup();
                resolve(files.length ? files : null);
            }
            function onCancel() {
                cleanup();
                resolve(null);
            }
            function onAbort() {
                cleanup();
                reject(signal.reason);
            }
            input.addEventListener('change', onChange);
            input.addEventListener('cancel', onCancel);
            signal?.addEventListener('abort', onAbort, { once: true });
            try {
                body.append(input);
                // Do not await anything before click: the browser owns the user-activation check.
                input.click();
            } catch (error) {
                cleanup();
                reject(error);
            }
        });
    }

    function clickDownload(href, fileName) {
        if (typeof fileName !== 'string' || !fileName.trim()) {
            throw new TypeError('A download file name is required');
        }
        const body = requireBody();
        const anchor = document.createElement('a');
        anchor.href = href;
        anchor.download = fileName;
        anchor.rel = 'noopener';
        anchor.hidden = true;
        try {
            body.append(anchor);
            anchor.click();
        } finally {
            anchor.remove();
        }
        return { status: 'handed-off' };
    }

    function downloadBlob(blob, fileName) {
        if (!(blob instanceof Blob)) throw new TypeError('A Blob is required for browser download');
        const objectUrl = urlApi.createObjectURL(blob);
        try {
            const result = clickDownload(objectUrl, fileName);
            // An immediate revoke can race the browser reading the Blob. This is URL cleanup,
            // not a timer guessing download completion or user cancellation.
            schedule(() => urlApi.revokeObjectURL(objectUrl), 60_000);
            return result;
        } catch (error) {
            urlApi.revokeObjectURL(objectUrl);
            throw error;
        }
    }

    /** The host supplies an opaque same-origin URL with Content-Disposition, never a disk path. */
    function downloadUrl(url, fileName) {
        const base = new URL(document.baseURI);
        const target = new URL(url, base);
        if (!['http:', 'https:'].includes(target.protocol) || target.origin !== base.origin
            || target.username || target.password) {
            throw new TypeError('Host downloads require an HTTP(S) same-origin URL');
        }
        return clickDownload(target.href, fileName);
    }

    return { pickFiles, downloadBlob, downloadUrl };
}
