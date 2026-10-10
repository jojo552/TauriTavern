import assert from 'node:assert/strict';
import test from 'node:test';
import { readFileSync } from 'node:fs';
import { Window } from 'happy-dom';

// The splash markup ships in index.html; drive the real markup with the real module.
test('boot splash progress only advances and the cover is removed after startup completes', async () => {
    const window = new Window({ url: 'http://localhost/' });
    const html = readFileSync(new URL('../../src/index.html', import.meta.url), 'utf8');
    const markup = html.slice(html.indexOf('<div id="preloader"'), html.indexOf('<div id="bg1">'));
    window.document.body.innerHTML = markup;
    const previous = { document: globalThis.document, setTimeout: globalThis.setTimeout };
    globalThis.document = window.document;
    const timers = [];
    globalThis.setTimeout = callback => timers.push(callback);
    try {
        const { setBootProgress, finishBootSplash } = await import('../../src/boot-splash.js');
        const splash = window.document.getElementById('preloader');
        const track = splash.querySelector('[role="progressbar"]');
        setBootProgress(0.48, 'Loading core data');
        setBootProgress(0.3, 'Earlier stage');
        assert.equal(track.getAttribute('aria-valuenow'), '48', 'progress never moves backwards');
        assert.equal(splash.querySelector('.tt-boot-splash__percent').textContent, '48%');
        assert.equal(splash.querySelector('.tt-boot-splash__text').textContent, 'Earlier stage');
        finishBootSplash();
        assert.equal(track.getAttribute('aria-valuenow'), '100');
        assert.equal(splash.getAttribute('aria-busy'), 'false');
        assert.ok(window.document.getElementById('preloader'), 'the cover fades before it is removed');
        timers.forEach(callback => callback());
        assert.equal(window.document.getElementById('preloader'), null);
        setBootProgress(0.9, 'late');
        finishBootSplash();
    } finally {
        globalThis.document = previous.document;
        globalThis.setTimeout = previous.setTimeout;
        await window.happyDOM.close();
    }
});
