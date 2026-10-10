import assert from 'node:assert/strict';
import test from 'node:test';
import { computeAccessibleName } from 'dom-accessibility-api';
import { createBrowserRuntime } from './runtime.mjs';

test('API presets, Select2 activation and key failures preserve their public behavior', async context => {
    const { window, load, getModule, startHost } = createBrowserRuntime();
    try {
        await startHost();
        await load('script.js');
        await load('scripts/select2-accessibility.js');
        const $ = window.jQuery;
        const script = getModule('script.js').namespace;
        const kai = getModule('scripts/kai-settings.js').namespace;
        const textgen = getModule('scripts/textgen-settings.js').namespace;
        const power = getModule('scripts/power-user.js').namespace;
        kai.initKoboldSettings();
        textgen.initTextGenSettings();

        await context.test('preset selection preserves legal context ranges and reports adjusted values once', async () => {
            power.prepareGenerationControls({ enableLabMode: false, max_context_unlocked: false });
            await power.loadPowerUserSettings({ power_user: { ...power.power_user } }, { themes: [], movingUIPresets: [], context: [] });
            const koboldPreset = { ...kai.kai_settings, temp: 1, genamt: 512, max_length: 65536, rep_pen_range: 32768 };
            const textgenPreset = { ...textgen.textgenerationwebui_settings, temp: 1, genamt: 512, max_length: 65536, rep_pen_range: 32768, dry_penalty_last_n: 32768 };
            kai.loadKoboldSettings({ koboldai_setting_names: ['Long'], koboldai_settings: [JSON.stringify(koboldPreset)] },
                { ...kai.kai_settings, preset_settings: 'Long' }, {});
            await textgen.loadTextGenSettings({ textgenerationwebui_preset_names: ['Long', 'Adjusted'], textgenerationwebui_presets: [
                JSON.stringify(textgenPreset), JSON.stringify({ ...textgenPreset, temp: 6, top_p: 2 }),
            ] }, { textgenerationwebui_settings: { ...textgen.textgenerationwebui_settings } });
            $('#settings_preset_textgenerationwebui').val('Long').trigger('change');
            assert.equal(textgen.textgenerationwebui_settings.rep_pen_range, 32768);
            assert.equal(textgen.textgenerationwebui_settings.dry_penalty_last_n, 32768);
            $('#max_context_unlocked').prop('checked', false).trigger('change');
            $('#settings_preset').val('0').trigger('change');
            assert.equal(kai.kai_settings.rep_pen_range, 32768);
            assert.equal(script.max_context, 65536);
            assert.equal($('.toast-warning').length, 0);

            $('#settings_preset_textgenerationwebui').val('Adjusted').trigger('change');
            assert.equal(textgen.textgenerationwebui_settings.temp, 5);
            assert.equal(textgen.textgenerationwebui_settings.top_p, 1);
            assert.equal($('.toast-warning').length, 1);
            assert.match($('.toast-warning').text(), /Text Completion/);
            assert.match($('.toast-warning').text(), /6 → 5/);
            assert.match($('.toast-warning').text(), /2 → 1/);
        });

        await context.test('Select2 keeps the field name and accepts native, generic and jQuery activation', () => {
            const source = $('#model_openrouter_select').empty().append('<option value="a">A</option><option value="b">B</option>');
            const name = computeAccessibleName(source[0], { hidden: true });
            source.select2();
            const trigger = source.next().find('.select2-selection')[0];
            assert.ok(name && computeAccessibleName(trigger, { hidden: true }).includes(name));
            for (const click of [element => element.click(), element => element.dispatchEvent(new window.Event('click', { bubbles: true })), element => $(element).trigger('click')]) {
                source.val('a').trigger('change');
                click(trigger);
                assert.equal(source.data('select2').isOpen(), true);
                click($('.select2-results__option').filter((_, item) => item.textContent === 'B')[0]);
                assert.equal(source.val(), 'b');
            }
            source.select2('destroy');
        });

        await context.test('parking a saved Custom source preserves its fields before and after source changes', async () => {
            const openai = getModule('scripts/openai.js').namespace;
            const gates = (await load('tauri/main/adapters/panel-runtime/api-connections-subtree-gates.js')).namespace;
            const { createEmbeddedRuntimeManager } = (await load('tauri/main/services/embedded-runtime/embedded-runtime-manager.js')).namespace;
            const { resolvePanelRuntimeProfile } = (await load('tauri/main/services/panel-runtime/panel-runtime-profiles.js')).namespace;

            // Gate listeners precede application handlers, as in host bootstrap. Loading saved
            // settings projects the form once; APP_READY then activates parking without a change.
            gates.preinstallApiConnectionsSubtreeGates();
            openai.initOpenAI();
            const saved = {
                ...structuredClone(openai.oai_settings),
                chat_completion_source: 'custom', custom_api_format: 'claude_messages',
                custom_url: 'https://custom.example', custom_model: 'saved-custom-model',
            };
            openai.loadOpenAISettings({ openai_setting_names: ['Saved'], openai_settings: [JSON.stringify(saved)] }, saved);
            $('#main_api').val('openai');
            const fields = ['custom_api_url_text', 'custom_model_id', 'api_key_custom']
                .map(id => window.document.getElementById(id));
            const manager = createEmbeddedRuntimeManager({ profile: resolvePanelRuntimeProfile('compat') });
            gates.activateApiConnectionsSubtreeGates({ manager });

            const assertCustomFields = () => {
                for (const field of fields) {
                    assert.equal(window.document.getElementById(field.id), field, field.id + ' must remain reachable');
                    assert.equal(field.isConnected, true);
                    assert.equal(window.document.getElementById('custom_form').contains(field), true);
                }
                assert.equal($('#custom_api_url_text').val(), saved.custom_url);
                assert.equal($('#custom_model_id').val(), saved.custom_model);
            };
            assertCustomFields();
            for (const source of ['custom_openai_responses', 'custom_gemini_generate_content', 'custom_gemini_interactions', 'custom']) {
                $('#chat_completion_source').val(source).trigger('change');
                assertCustomFields();
            }
            $('#chat_completion_source').val('openai').trigger('change');
            assert.equal(fields.every(field => !field.isConnected), true);
            $('#chat_completion_source').val('custom_claude_messages').trigger('change');
            assertCustomFields();
            // The following key test exercises the ordinary OpenAI form.
            $('#chat_completion_source').val('openai').trigger('change');
        });

        await context.test('a saved key rejects on refresh failure and clears the input to avoid a duplicate write', async () => {
            const secrets = getModule('scripts/secrets.js').namespace;
            $('#api_key_openai').val('TEST-ONLY');
            window.fetch = async url => {
                if (url === '/api/secrets/write') return window.Response.json({ id: 'saved' });
                assert.equal(url, '/api/secrets/read');
                return new window.Response('Test refresh failure', { status: 500 });
            };
            await assert.rejects(secrets.writeSecret('api_key_openai', 'TEST-ONLY', 'Test key'), /refresh API key state/);
            assert.equal($('#api_key_openai').val(), '');
        });
    } finally {
        getModule('script.js').namespace.cancelPendingSettingsSave();
        await window.happyDOM.close();
    }
});
