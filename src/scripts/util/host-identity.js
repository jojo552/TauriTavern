// @ts-check

/** @typedef {'windows' | 'macos' | 'linux' | 'android' | 'ios' | 'ohos' | 'browser'} HostPlatform */
/** @typedef {{ platform: HostPlatform, kind: 'desktop' | 'mobile' | 'web' }} HostIdentity */

/** @returns {Readonly<HostIdentity>} */
function identity() {
    const value = globalThis.__TAURITAVERN_HOST__;
    if (!value) {
        throw new Error('TauriTavern host identity is missing');
    }
    return value;
}

// Recognition also runs outside a project host, before bridge initialization.
export function isWebHost() {
    const value = globalThis.__TAURITAVERN_HOST__;
    return value?.platform === 'browser' && value?.kind === 'web';
}

export function hostPlatform() { return identity().platform; }
export function isMobileHost() { return identity().kind === 'mobile'; }
export function isDesktopHost() { return identity().kind === 'desktop'; }
