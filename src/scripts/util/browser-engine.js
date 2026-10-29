// @ts-check

/** @typedef {{ brands: readonly { brand: string }[] }} UserAgentHints */

/**
 * The browser host names no engine: one platform runs whichever engine the user's browser has. Only Chromium exposes
 * user-agent client hints, and only in secure contexts; Gecko and WebKit, which every iOS browser uses, expose none.
 * Without hints, callers get the behavior that is correct in every engine.
 */
export function isChromiumEngine() {
    const hints = /** @type {{ userAgentData?: UserAgentHints }} */ (/** @type {unknown} */ (navigator)).userAgentData;
    return hints?.brands.some(({ brand }) => brand === 'Chromium') ?? false;
}
