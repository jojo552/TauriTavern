// A private HTTP implementation of the common native/Studio bridge interface.
// No Tauri globals, plugin emulation or alternate application services are installed.
import { createWebTransport, WebAuthenticationError } from './transport.js';
import { isWebHost } from '../scripts/util/host-identity.js';

export function createWebHostApi() {
    let transport;
    function getTransport() {
        if (!isWebHost()) throw new Error('Web transport requires an explicit browser/web host identity');
        if (transport) return transport;
        transport = createWebTransport({
            onError(error) {
                if (globalThis.reportError) globalThis.reportError(error);
                else console.error(error);
                if (error instanceof WebAuthenticationError) {
                    // Preserve unsaved work. Signing back in must not replay the failed command.
                    globalThis.toastr?.warning?.('登录已失效。点击打开登录页；重新登录后，请先复制未保存内容，再刷新本页。', '需要重新登录', {
                        timeOut: 0, extendedTimeOut: 0,
                        onclick: () => window.open('/login', '_blank', 'noopener,noreferrer'),
                    });
                }
            },
        });
        return transport;
    }

    return {
        get transport() { return getTransport(); },
        core: {
            invoke: (...args) => getTransport().invoke(...args),
            Channel: class {
                constructor(onmessage) { return getTransport().createChannel(onmessage); }
            },
        },
        event: {
            listen: (...args) => getTransport().listen(...args),
        },
        writeClipboardText(text) {
            if (typeof navigator.clipboard?.writeText !== 'function') {
                throw new Error('Browser clipboard requires a secure context and clipboard permission');
            }
            return navigator.clipboard.writeText(text);
        },
        openExternalUrl(href) {
            const openedWindow = typeof window.open === 'function'
                ? window.open(href, '_blank', 'noopener,noreferrer')
                : null;
            if (openedWindow) return;
            if (typeof window.location?.assign === 'function') {
                window.location.assign(href);
                return;
            }
            throw new Error('Unable to open external URL');
        },
    };
}
