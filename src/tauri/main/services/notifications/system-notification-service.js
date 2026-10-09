// @ts-check

/**
 * @typedef {'granted' | 'denied' | 'prompt'} NotificationPermissionState
 * @typedef {(command: import('../../context/types.js').TauriInvokeCommand, args?: any) => Promise<any>} SafeInvokeFn
 */

const NOTIFICATION_PERMISSION_STATES = new Set(['granted', 'denied', 'prompt']);
const NOTIFICATION_PERMISSION_REJECTION_COUNT_STORAGE_KEY = 'tt:notification-permission-rejection-count';
const NOTIFICATION_PERMISSION_REJECTION_LIMIT = 3;

/**
 * @param {unknown} value
 * @returns {NotificationPermissionState}
 */
function normalizePermissionState(value) {
    const normalized = String(value || '').trim().toLowerCase();
    if (NOTIFICATION_PERMISSION_STATES.has(normalized)) {
        return /** @type {NotificationPermissionState} */ (normalized);
    }

    throw new Error(`Unsupported notification permission state: ${String(value || '')}`);
}

/**
 * @param {Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>} storage
 * @returns {number}
 */
function getPermissionRejectionCount(storage) {
    const raw = storage.getItem(NOTIFICATION_PERMISSION_REJECTION_COUNT_STORAGE_KEY);
    const count = Number.parseInt(String(raw ?? ''), 10);
    return Number.isSafeInteger(count) && count > 0 ? count : 0;
}

/**
 * @param {Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>} storage
 * @param {number} count
 */
function setPermissionRejectionCount(storage, count) {
    if (count <= 0) {
        storage.removeItem(NOTIFICATION_PERMISSION_REJECTION_COUNT_STORAGE_KEY);
        return;
    }

    storage.setItem(NOTIFICATION_PERMISSION_REJECTION_COUNT_STORAGE_KEY, String(count));
}

/**
 * The browser host has no native notification plugin: the page itself owns the permission and
 * shows notifications through the Web Notification API. Permission states map one to one
 * (`default` is the browser's name for `prompt`).
 *
 * @param {typeof globalThis.Notification | undefined} BrowserNotification
 * @returns {SafeInvokeFn}
 */
export function createBrowserNotificationInvoke(BrowserNotification = globalThis.Notification) {
    /** @param {string} permission */
    const toState = (permission) => (permission === 'default' ? 'prompt' : permission);
    return async (command, args) => {
        if (typeof BrowserNotification !== 'function') {
            // Insecure origins and some embedded browsers have no Notification API at all.
            if (command === 'show_system_notification') {
                throw new Error('This browser does not support system notifications');
            }
            return 'denied';
        }
        switch (command) {
            case 'get_notification_permission_state':
                return toState(BrowserNotification.permission);
            case 'request_notification_permission':
                return toState(await BrowserNotification.requestPermission());
            case 'show_system_notification': {
                if (BrowserNotification.permission !== 'granted') {
                    throw new Error('Notification permission has not been granted');
                }
                new BrowserNotification(args.dto.title, { body: args.dto.body });
                return null;
            }
            default:
                throw new Error(`Unsupported browser notification command: ${command}`);
        }
    };
}

/**
 * @param {{
 *   safeInvoke: SafeInvokeFn;
 *   confirmPermissionRationale: () => Promise<boolean>;
 *   storage?: Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;
 * }} deps
 */
export function createSystemNotificationService({
    safeInvoke,
    confirmPermissionRationale,
    storage = globalThis.localStorage,
}) {
    /** @type {Promise<NotificationPermissionState> | null} */
    let permissionRequestPromise = null;
    /** @type {Promise<boolean> | null} */
    let permissionRationalePromise = null;

    function resetPermissionRejectionCount() {
        setPermissionRejectionCount(storage, 0);
    }

    function incrementPermissionRejectionCount() {
        const nextCount = getPermissionRejectionCount(storage) + 1;
        setPermissionRejectionCount(storage, nextCount);
        return nextCount;
    }

    async function getPermissionState() {
        return normalizePermissionState(await safeInvoke('get_notification_permission_state'));
    }

    async function requestPermission() {
        if (!permissionRequestPromise) {
            permissionRequestPromise = safeInvoke('request_notification_permission')
                .then(normalizePermissionState)
                .then((state) => {
                    if (state === 'granted') {
                        resetPermissionRejectionCount();
                        return state;
                    }

                    incrementPermissionRejectionCount();
                    return state;
                })
                .finally(() => {
                    permissionRequestPromise = null;
                });
        }

        return permissionRequestPromise;
    }

    async function confirmPermissionRationaleOnce() {
        if (getPermissionRejectionCount(storage) >= NOTIFICATION_PERMISSION_REJECTION_LIMIT) {
            return false;
        }

        if (!permissionRationalePromise) {
            permissionRationalePromise = confirmPermissionRationale()
                .then((accepted) => {
                    if (!accepted) {
                        incrementPermissionRejectionCount();
                    }

                    return accepted;
                })
                .finally(() => {
                    permissionRationalePromise = null;
                });
        }

        return permissionRationalePromise;
    }

    async function preparePermission() {
        const currentState = await getPermissionState();
        if (currentState === 'granted') {
            resetPermissionRejectionCount();
            return currentState;
        }

        if (currentState !== 'prompt') {
            return currentState;
        }

        const accepted = await confirmPermissionRationaleOnce();
        if (!accepted) {
            return currentState;
        }

        return requestPermission();
    }

    /**
     * @param {{ title: string; body: string }} params
     */
    async function show({ title, body }) {
        await safeInvoke('show_system_notification', {
            dto: {
                title: String(title ?? '').trim(),
                body: String(body ?? '').trim(),
            },
        });
    }

    return {
        getPermissionState,
        requestPermission,
        preparePermission,
        show,
    };
}
