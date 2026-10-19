import assert from 'node:assert/strict';
import test from 'node:test';
import { createBrowserRuntime } from './runtime.mjs';

test('backend errors wait for the application consumer, deduplicate and render untrusted messages as text', async () => {
    const { window, host, load, startHost } = await createBrowserRuntime();
    try {
        await startHost();
        const emit = message => host.emit('tauritavern-backend-error', { message });
        const earlyMarkup = '<img src="test-only" onerror="alert(1)">';
        emit('Early maintenance failure');
        emit('Early maintenance failure');
        emit(earlyMarkup);
        assert.equal(window.document.querySelectorAll('.toast-error').length, 0);

        await load('script.js');
        const messages = () => Array.from(window.document.querySelectorAll('.toast-error .toast-message'));
        assert.deepEqual(messages().map(element => element.textContent).sort(), ['Early maintenance failure', earlyMarkup].sort());
        assert.equal(messages().find(element => element.textContent === earlyMarkup).querySelector('img'), null);

        // An extension can alter global toastr options; this error channel must still be text-only.
        window.toastr.options.escapeHtml = false;
        const liveMarkup = '<b>Live backend failure</b>';
        emit(liveMarkup);
        emit(liveMarkup);
        assert.equal(messages().length, 3);
        const live = messages().find(element => element.textContent === liveMarkup);
        assert.ok(live);
        assert.equal(live.querySelector('b'), null);
    } finally {
        await window.happyDOM.close();
    }
});
