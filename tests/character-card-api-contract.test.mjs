import test from 'node:test';
import assert from 'node:assert/strict';
import { installCharacterCardsApi, LorebookAdoptionConflictError } from '../src/tauri/main/api/character-cards.js';
import { createInvokeService } from '../src/tauri/main/services/invokes/invoke-service.js';
import { HOSTS, installHostIdentity } from './helpers/host-identity.mjs';

function installHarness({ files, readFile }) {
    const discarded = [];
    globalThis.window = { __TAURITAVERN__: { api: {} } };
    installCharacterCardsApi({
        invalidateCharacterCache: () => {},
        safeInvoke: async (command, args) => {
            if (command === 'pick_import_files') {
                return files;
            }
            if (command === 'stage_file_discard') {
                discarded.push(args.filePath);
                return;
            }
            throw new Error(`Unexpected command: ${command}`);
        },
        createReadableFileStream: async path => new ReadableStream({
            start(controller) {
                controller.enqueue(readFile(path));
                controller.close();
            },
        }),
    });
    return { characterCards: window.__TAURITAVERN__.api.characterCards, discarded };
}

test('native character selection preserves original names and bytes while releasing staged copies', async () => {
    const selected = [
        { path: '/cache/one.json', name: 'Alice%20.json' },
        { path: '/cache/two.png', name: 'Alice Smith.png' },
    ];
    const png = new Uint8Array([137, 80, 78, 71]);
    const { characterCards, discarded } = installHarness({
        files: selected,
        readFile: path => path.endsWith('.png') ? png : new TextEncoder().encode('{"name":"Alice"}'),
    });
    const files = await characterCards.pickFiles({ multiple: true });
    assert.deepEqual(files.map(file => [file.name, file.type]), [
        ['Alice%20.json', 'application/json'], ['Alice Smith.png', 'image/png'],
    ]);
    assert.equal(await files[0].text(), '{"name":"Alice"}');
    assert.deepEqual(new Uint8Array(await files[1].arrayBuffer()), png);
    assert.deepEqual(discarded, selected.map(file => file.path));
});

for (const host of [HOSTS.windows, { platform: 'browser', kind: 'web' }]) {
    test(`${host.kind} character cards expose lorebook adoption and reject stale decisions without repeating writes`, async (t) => {
        t.after(installHostIdentity(host));
        const calls = [];
        let invalidations = 0;
        const adopted = { status: 'decision', world: 'Library', target: 'Library', can_update: true, token: 'current' };
        const resolved = { status: 'linked', world: 'Library', previous: '', created: false, updated: true };
        const { safeInvoke } = createInvokeService({
            policies: {},
            invoke: async (command, args) => {
                calls.push({ command, args });
                if (command === 'adopt_character_lorebook') return adopted;
                if (command === 'resolve_character_lorebook_adoption') {
                    if (args.dto.token === 'stale') throw { Conflict: 'Lorebook decision expired' };
                    return resolved;
                }
                throw new Error(`Unexpected command: ${command}`);
            },
        });
        globalThis.window = { __TAURITAVERN__: { api: {} } };
        installCharacterCardsApi({
            safeInvoke,
            invalidateCharacterCache: () => { invalidations++; },
            createReadableFileStream: () => { throw new Error('Lorebook adoption must not read a picked file'); },
        });
        const cards = window.__TAURITAVERN__.api.characterCards;
        assert.equal(await cards.adoptLorebook('Alice%20#1.png'), adopted);
        assert.equal(await cards.resolveLorebookAdoption('Alice%20#1.png', 'update', 'current'), resolved);
        assert.equal(invalidations, 2);
        await assert.rejects(
            cards.resolveLorebookAdoption('Alice%20#1.png', 'adopt', 'stale'),
            error => error instanceof LorebookAdoptionConflictError && error.message.includes('Lorebook decision expired'),
        );
        assert.equal(invalidations, 2, 'a rejected decision cannot publish a successful cache mutation');
        assert.deepEqual(calls, [
            { command: 'adopt_character_lorebook', args: { dto: { name: 'Alice%20#1' } } },
            { command: 'resolve_character_lorebook_adoption', args: { dto: { name: 'Alice%20#1', choice: 'update', token: 'current' } } },
            { command: 'resolve_character_lorebook_adoption', args: { dto: { name: 'Alice%20#1', choice: 'adopt', token: 'stale' } } },
        ]);
    });
}
