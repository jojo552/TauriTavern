// The boot splash is the page's initial cover (#preloader in index.html). Startup owns it:
// stages advance a monotonic progress bar, and the cover fades out only once everything,
// including deferred extensions and the last chat, has loaded. No dependencies: init.js uses it
// before any library is available.

const SPLASH_ID = 'preloader';
const FADE_MS = 420;

/**
 * Advances the progress bar. Progress never moves backwards; a missing splash (already finished,
 * or replaced by the startup error panel) has nothing left to update.
 * @param {number} fraction Completed share of startup, 0..1
 * @param {string} [text] Localized description of the current stage
 */
export function setBootProgress(fraction, text) {
    const splash = document.getElementById(SPLASH_ID);
    const track = splash?.querySelector('.tt-boot-splash__track');
    if (!splash || !track) return;
    const current = Number(track.getAttribute('aria-valuenow')) || 0;
    const percent = Math.max(current, Math.min(100, Math.round(fraction * 100)));
    track.setAttribute('aria-valuenow', String(percent));
    splash.style.setProperty('--tt-boot-progress', String(percent / 100));
    splash.querySelector('.tt-boot-splash__percent').textContent = `${percent}%`;
    if (text !== undefined) {
        splash.querySelector('.tt-boot-splash__text').textContent = text;
    }
}

/** Completes the bar, fades the cover out and removes it, revealing the loaded interface. */
export function finishBootSplash() {
    const splash = document.getElementById(SPLASH_ID);
    if (!splash) return;
    setBootProgress(1);
    splash.setAttribute('aria-busy', 'false');
    splash.classList.add('tt-boot-splash--done');
    // A timer matching the CSS fade is deterministic; transitionend may never fire.
    setTimeout(() => splash.remove(), FADE_MS);
}
