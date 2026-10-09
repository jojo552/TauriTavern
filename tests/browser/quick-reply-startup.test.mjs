import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { createBrowserRuntime } from './runtime.mjs';

const settingsTemplate = readFileSync(new URL('../../src/scripts/extensions/quick-reply/html/settings.html', import.meta.url), 'utf8');

async function withQuickReplies(run) {
    const { window, getModule, load } = createBrowserRuntime();
    try {
        const main = (await load('script.js')).namespace;
        const extensions = getModule('scripts/extensions.js').namespace;
        const feature = (await load('scripts/extensions/quick-reply/index.js')).namespace;
        const { QuickReplySet } = getModule('scripts/extensions/quick-reply/src/QuickReplySet.js').namespace;
        let settingsReads = 0;
        let response = () => window.Response.json({ quickReplyPresets: [] });
        window.fetch = async url => {
            if (url === '/api/settings/get') {
                settingsReads++;
                return response();
            }
            assert.equal(url, '/scripts/extensions/quick-reply/html/settings.html');
            return new window.Response(settingsTemplate);
        };
        await run({
            window, main, extensions, feature, QuickReplySet,
            settingsReads: () => settingsReads,
            setResponse: next => { response = next; },
        });
    } finally {
        await window.happyDOM.close();
    }
}

const linkedPresets = () => [{
    name: 'Legacy', version: 1, quickActionEnabled: true, placeBeforeInputEnabled: true, AutoInputInject: true,
    quickReplySlots: [{
        label: 'Greeting', title: 'Legacy title', mes: 'Hello', hidden: false,
        autoExecute_userMessage: true, automationId: 'legacy-greeting',
        contextMenu: [{ preset: 'Later', chain: true }],
    }],
}, {
    name: 'Later', version: 2, qrList: [{ id: 7, label: 'Follow-up', message: 'Continue', contextList: [] }],
}];

function assertLinkedPresets(QuickReplySet) {
    const legacy = QuickReplySet.get('Legacy');
    const later = QuickReplySet.get('Later');
    assert.equal(legacy.disableSend, true);
    assert.equal(legacy.placeBeforeInput, true);
    assert.equal(legacy.injectInput, true);
    const greeting = legacy.qrList[0];
    assert.equal(greeting.id, 1);
    assert.equal(greeting.label, 'Greeting');
    assert.equal(greeting.title, 'Legacy title');
    assert.equal(greeting.message, 'Hello');
    assert.equal(greeting.executeOnUser, true);
    assert.equal(greeting.automationId, 'legacy-greeting');
    assert.equal(greeting.contextList[0].set, later, 'Forward context links resolve after every set is registered');
    assert.equal(greeting.contextList[0].isChained, true);
}

test('quick-reply activation consumes the startup presets once without reading settings again', async () => {
    await withQuickReplies(async ({ window, main, extensions, feature, QuickReplySet, settingsReads }) => {
        extensions.extension_settings.quickReply = { quickReplyEnabled: true, selectedPreset: 'Legacy' };
        main.primeQuickReplyPresetsSnapshot(linkedPresets());
        await feature.init();
        assert.equal(settingsReads(), 0);
        assertLinkedPresets(QuickReplySet);
        assert.equal(feature.quickReplyApi.settings.config.setList[0].set, QuickReplySet.get('Legacy'));
        await window.executeQuickReplyByName('Legacy.Greeting');
        assert.equal(window.document.querySelector('#send_textarea').value, 'Hello ', 'The migrated reply executes through its initialized set');
        assert.equal(main.consumeQuickReplyPresetsSnapshot(), null, 'Consumed startup data is no longer retained');
    });
});

test('empty startup presets are consumed, and a subsequent activation reads fresh settings', async () => {
    await withQuickReplies(async ({ window, main, extensions, feature, QuickReplySet, settingsReads, setResponse }) => {
        main.primeQuickReplyPresetsSnapshot([]);
        await feature.init();
        assert.equal(settingsReads(), 0, 'An empty snapshot is a loaded result, not a cache miss');
        assert.equal(QuickReplySet.list.length, 0);

        extensions.extension_settings.quickReplyV2 = { isEnabled: false, config: { setList: [] } };
        setResponse(() => window.Response.json({ quickReplyPresets: linkedPresets() }));
        await feature.init();
        assert.equal(settingsReads(), 1);
        assertLinkedPresets(QuickReplySet);
    });
});

test('discarded startup presets cannot be reused by later quick-reply activation', async () => {
    await withQuickReplies(async ({ window, main, feature, QuickReplySet, settingsReads, setResponse }) => {
        main.primeQuickReplyPresetsSnapshot([{ name: 'Stale', version: 2, qrList: [] }]);
        main.primeQuickReplyPresetsSnapshot(null);
        setResponse(() => window.Response.json({ quickReplyPresets: linkedPresets() }));
        await feature.init();
        assert.equal(settingsReads(), 1);
        assert.equal(QuickReplySet.get('Stale'), undefined);
        assertLinkedPresets(QuickReplySet);
    });
});

test('quick-reply settings failures reject activation without retrying or using stale presets', async () => {
    await withQuickReplies(async ({ window, main, feature, QuickReplySet, settingsReads, setResponse }) => {
        setResponse(() => new window.Response('Unavailable', { status: 503 }));
        await assert.rejects(feature.init(), /Quick Reply settings request failed with status 503/);
        assert.equal(settingsReads(), 1);
        assert.equal(QuickReplySet.list.length, 0);

        setResponse(() => new window.Response('{broken', { headers: { 'Content-Type': 'application/json' } }));
        await assert.rejects(feature.init(), { name: 'SyntaxError' });
        assert.equal(settingsReads(), 2, 'A parse failure is not silently retried');

        main.primeQuickReplyPresetsSnapshot([{ name: 'Malformed', version: 1 }]);
        await assert.rejects(feature.init(), { name: 'TypeError' });
        assert.equal(settingsReads(), 2, 'An invalid startup snapshot must not fall back to a second settings load');
        assert.equal(main.consumeQuickReplyPresetsSnapshot(), null);
    });
});
