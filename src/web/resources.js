// Numeric resource handles remain stable across a brief reconnect, but never alias a resource
// created by a new server session. Callers keep the same public numeric-handle contract.
export function createWebResources() {
    let nextHandle = 0;
    const handles = new Map();

    function add(id, kind) {
        if (!Number.isInteger(id) || id < 0) throw new Error('Invalid host resource id');
        const handle = ++nextHandle;
        if (handle > 0xffffffff) throw new Error('Browser resource handle limit reached; reload the page');
        handles.set(handle, { id, kind });
        return handle;
    }

    function resolve(handle, kind) {
        const resource = handles.get(handle);
        if (!resource || (kind && resource.kind !== kind)) {
            throw new Error('Host resource expired; reopen the chat or file before continuing');
        }
        return resource.id;
    }

    function argumentsFor(command, args) {
        if (command === 'read_bytes' || command === 'close_resource') {
            return { ...args, rid: resolve(args.rid, command === 'read_bytes' ? 'reader' : undefined) };
        }
        if (command === 'open_cold_swipe_record') {
            return { ...args, sourceId: resolve(args.sourceId ?? args.source_id, 'swipe') };
        }
        const operation = args?.operation;
        const coldSource = operation?.coldSourceId ?? operation?.cold_source_id;
        if (command === 'begin_chat_commit' && coldSource != null) {
            return { ...args, operation: { ...operation, coldSourceId: resolve(coldSource, 'swipe') } };
        }
        return args;
    }

    function accept(command, value) {
        if (value == null) return value;
        switch (command) {
            case 'open_chat_reader':
            case 'open_chat_payload_json':
            case 'open_chat_backup_download':
            case 'open_cold_swipe_record':
                return add(value, 'reader');
            case 'open_extension_store_entry':
                return { ...value, readerId: add(value.readerId, 'reader') };
            case 'open_cold_chat':
                return { ...value, sourceId: add(value.sourceId, 'swipe'), readerId: add(value.readerId, 'reader') };
            default:
                return value;
        }
    }

    return {
        argumentsFor, accept,
        release: handle => handles.delete(handle),
        reset: () => handles.clear(),
    };
}
