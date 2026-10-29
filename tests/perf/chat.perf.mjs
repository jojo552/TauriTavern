import { createHostInvokePolicies } from '../../src/tauri/main/kernel/invokes/invoke-policies.js';

// Token counts stay cached for a span of wall-clock time. The step waits it out before sending, after the counts
// of earlier steps, and before ending the reply, after the prompt's counts, so every count reaches the host however
// fast the machine runs.
const { cacheTtlMs } = createHostInvokePolicies().count_openai_tokens_batch;
const outliveTokenCounts = () => new Promise(resolve => setTimeout(resolve, cacheTtlMs));

/** Chat: sending a message, then rendering its reply token by token. */
export default async function chat({ page, measure }) {
    await page.evaluate(() => window.jQuery('#api_button_openai').triggerHandler('click'));
    await page.focus('#send_textarea');
    const tokens = 'The keeper lifts the lantern and counts the ships.'.split(' ');
    // Each token is its own input, so the streaming frame-rate limit, timed by the wall clock, renders every one.
    await measure('chat/send-and-stream',
        async () => {
            await outliveTokenCounts();
            await page.keyboard.insertText('Tell me about the reef.');
            await page.keyboard.press('Enter');
        },
        ...tokens.map(token => () => page.evaluate(text => globalThis.__perfReply.push(text), `${token} `)),
        async () => {
            await outliveTokenCounts();
            await page.evaluate(() => globalThis.__perfReply.end());
            await page.evaluate(async () => (await import('/script.js')).waitForGenerationIdle());
        },
    );
}
