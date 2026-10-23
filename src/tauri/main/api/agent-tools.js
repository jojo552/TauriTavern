// @ts-check

import { createChannel, onHostSessionReset } from '../../../tauri-bridge.js';
import { encodeBytesToBase64 } from '../binary-utils.js';

/**
 * @param {{
 *   safeInvoke: (command: string, args?: any) => Promise<any>;
 *   channelFactory?: (onmessage: (event: any) => void) => any;
 *   sessionReset?: (handler: () => void | Promise<void>) => () => void;
 * }} deps
 * @returns {TauriTavernAgentToolsApi}
 */
export function createAgentToolsApi({ safeInvoke, channelFactory = createChannel, sessionReset = onHostSessionReset }) {
    /** @type {Map<string, any>} Confirmed declarative registrations, not an invoke retry queue. */
    const registrations = new Map();
    /** @type {Set<Map<string, AbortController>>} Includes calls whose registration is not yet acknowledged. */
    const activeCalls = new Set();
    sessionReset(async () => {
        for (const pending of activeCalls) {
            for (const controller of pending.values()) controller.abort();
            pending.clear();
        }
        const results = await Promise.allSettled([...registrations.values()].map(registration => registration.attach()));
        const failures = results.filter(result => result.status === 'rejected');
        if (failures.length) throw new AggregateError(failures.map(result => result.reason), 'Failed to restore registered extension tools');
    });
    return {
        list(options) {
            return options === undefined
                ? safeInvoke('list_agent_tools')
                : safeInvoke('list_agent_tools', { dto: options });
        },
        async setEnabled(toolId, enabled) {
            await safeInvoke('set_agent_extension_tool_enabled', { dto: { toolId, enabled } });
            const registration = registrations.get(toolId);
            if (registration) registration.definition.enabled = enabled;
        },
        async register(definition, execute) {
            if (typeof execute !== 'function') throw new TypeError('An extension tool execute function is required');
            /** @type {Map<string, AbortController>} */
            const pending = new Map();
            activeCalls.add(pending);

            /** @param {{ requestId: string; call: Omit<TauriTavernExtensionToolContext, 'signal' | 'attachImage'> & { arguments: Record<string, TauriTavernJsonValue> } }} event */
            async function executeCall({ requestId, call }) {
                const controller = new AbortController();
                pending.set(requestId, controller);
                let result;
                try {
                    /** @type {Blob[]} */
                    const attached = [];
                    const value = await execute(call.arguments, {
                        runId: call.runId,
                        invocationId: call.invocationId,
                        callId: call.callId,
                        target: call.target,
                        signal: controller.signal,
                        attachImage: image => { attached.push(image); },
                    });
                    const images = await Promise.all(attached.map(async image => ({ data: encodeBytesToBase64(await image.arrayBuffer()) })));
                    result = { kind: 'value', value: value === undefined ? null : value, images };
                    // Classify local serialization failure before attempting the one result submission.
                    JSON.stringify(result);
                } catch (error) {
                    result = { kind: 'error', message: error instanceof Error ? error.message : String(error) };
                }
                try {
                    if (!pending.has(requestId)) return;
                    // A lost receipt may mean this result already committed. Never submit an error in its place.
                    await safeInvoke('resolve_agent_extension_tool_call', { dto: { requestId, result } });
                } finally {
                    pending.delete(requestId);
                }
            }

            const registration = {
                definition: structuredClone(definition),
                pending,
                async attach() {
                    let accepting = true;
                    const channel = channelFactory(event => {
                        if (!accepting) return;
                        if (event.type === 'cancel') {
                            const controller = pending.get(event.requestId);
                            pending.delete(event.requestId);
                            controller?.abort();
                            return;
                        }
                        return executeCall(event).catch(error => {
                            console.error('[AgentTools] Failed to submit extension tool result', error);
                        });
                    });
                    try { await safeInvoke('register_agent_extension_tool', { definition: registration.definition, channel }); }
                    catch (error) {
                        accepting = false; // An unacknowledged registration cannot execute calls later.
                        for (const controller of pending.values()) controller.abort();
                        pending.clear();
                        throw error;
                    }
                },
            };
            try { await registration.attach(); }
            catch (error) {
                for (const controller of pending.values()) controller.abort();
                pending.clear();
                activeCalls.delete(pending);
                throw error;
            }
            registrations.set(`extension/${definition.extensionId.trim()}:${definition.name.trim()}`, registration);
        },
    };
}
