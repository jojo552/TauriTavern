// @ts-check
import { encodeBytesToBase64 } from '../../binary-utils.js';
import { hostPlatform } from '../../../../scripts/util/host-identity.js';

// Below this size a request body crosses the network in about one round trip either way.
const BROWSER_GZIP_MIN_BYTES = 32 * 1024;

/** @param {Uint8Array} bytes */
async function gzip(bytes) {
    const stream = new Blob([/** @type {Uint8Array<ArrayBuffer>} */ (bytes)]).stream().pipeThrough(new CompressionStream('gzip'));
    return new Uint8Array(await new Response(stream).arrayBuffer());
}

/**
 * Encode the byte body shared by upload and commit commands.
 * The browser host reaches Rust over a network instead of in-process IPC, so text bodies are sent
 * with HTTP `Content-Encoding: gzip` and decoded by the server before the command sees them.
 * Offsets and sizes stay in decoded bytes. Already-compressed uploads (images, archives) gain
 * nothing, so callers opt in only for text.
 * @param {Uint8Array} bytes
 * @param {{ compressible?: boolean }} [options]
 * @returns {Promise<{ body: Uint8Array | { data: string }, headers: Record<string, string> }>}
 */
export async function encodeIpcByteBody(bytes, { compressible = false } = {}) {
    switch (hostPlatform()) {
        case 'android':
            return { body: { data: encodeBytesToBase64(bytes) }, headers: { 'chunk-encoding': 'base64' } };
        case 'browser':
            if (compressible && bytes.byteLength >= BROWSER_GZIP_MIN_BYTES) {
                return { body: await gzip(bytes), headers: { 'content-encoding': 'gzip' } };
            }
            return { body: bytes, headers: {} };
        case 'windows':
        case 'macos':
        case 'linux':
        case 'ios':
        case 'ohos':
            return { body: bytes, headers: {} };
        default:
            throw new Error(`Unsupported byte transport platform: ${hostPlatform()}`);
    }
}
