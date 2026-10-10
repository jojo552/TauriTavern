import { extractErrorText } from './tauri/main/kernel/host-error-response.js';

const FAILURE_STYLE = `
    section { max-width: 720px; margin: 0 auto; }
    h1 { margin: 0 0 16px; font-size: 24px; }
    pre { max-height: 50vh; overflow: auto; padding: 12px; border-radius: 6px;
        background: #303030; font: 13px/1.5 ui-monospace, monospace;
        white-space: pre-wrap; overflow-wrap: anywhere; user-select: text; -webkit-user-select: text; }
    .actions { display: flex; flex-wrap: wrap; gap: 12px; }
    button { padding: 8px 16px; font: inherit; cursor: pointer; }
    .copy-status { white-space: pre-wrap; overflow-wrap: anywhere; }
`;

// Available before lib.js and script.js: startup failure must not depend on the UI it failed to load.
export function showStartupError(error) {
    document.getElementById('preloader')?.remove();
    document.getElementById('tt-startup-error')?.remove();
    const stage = globalThis.__TAURITAVERN_STARTUP_STAGE__;
    const details = [extractErrorText(error), stage && `Stage: ${stage}`, error?.stack]
        .filter(Boolean).join('\n');
    const panel = document.createElement('section');
    panel.id = 'tt-startup-error';
    panel.setAttribute('role', 'alert');
    // Page selectors outrank :host, so reset the fullscreen host inline and isolate its children.
    panel.style.cssText = 'all:initial;position:fixed;inset:0;z-index:2147483647;box-sizing:border-box;overflow:auto;padding:24px;background:#202020;color:#fff;font:16px/1.5 system-ui;';
    const root = panel.attachShadow({ mode: 'open' });
    root.innerHTML = `<style>${FAILURE_STYLE}</style><section>
        <h1>TauriTavern could not start</h1>
        <pre></pre>
        <p>Startup has stopped. Resolve the error, then reload to try again.</p>
        <div class="actions"><button type="button" name="copy">Copy details</button>
            <button type="button" name="reload">Reload</button></div>
        <p class="copy-status" role="status"></p>
    </section>`;
    root.querySelector('pre').textContent = details;
    const copy = root.querySelector('button[name=copy]');
    const status = root.querySelector('.copy-status');
    copy.addEventListener('click', async () => {
        copy.disabled = true;
        status.textContent = '';
        try {
            // The clipboard bridge may depend on the failed host; load it only for this user action.
            const { writeClipboardText } = await import('./tauri-bridge.js');
            await writeClipboardText(details);
            status.textContent = 'Copied!';
        } catch (copyError) {
            console.error('TauriTavern: Could not copy startup details:', copyError);
            status.textContent = `Could not copy details: ${extractErrorText(copyError)}. Select the text above to copy it manually.`;
        } finally {
            copy.disabled = false;
        }
    });
    const reload = root.querySelector('button[name=reload]');
    reload.addEventListener('click', () => window.location.reload());
    document.body.appendChild(panel);
    reload.focus();
}
