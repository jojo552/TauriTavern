/** Chat: sending a message, then rendering its reply token by token. */
export default async function chat({ page, measure }) {
    await page.evaluate(() => window.jQuery('#api_button_openai').triggerHandler('click'));
    await page.focus('#send_textarea');
    const tokens = 'The keeper lifts the lantern and counts the ships.'.split(' ');
    // Each token is its own input, so the streaming frame-rate limit, timed by the wall clock, renders every one.
    await measure('chat/send-and-stream',
        async () => {
            await page.keyboard.insertText('Tell me about the reef.');
            await page.keyboard.press('Enter');
        },
        ...tokens.map(token => () => page.evaluate(text => globalThis.__perfReply.push(text), `${token} `)),
        async () => {
            await page.evaluate(watchPromptPreview);
            await page.evaluate(() => globalThis.__perfReply.end());
            await page.evaluate(async () => (await import('/script.js')).waitForGenerationIdle());
        },
        () => promptPreview(page),
    );
}

// The reply refreshes the prompt manager's preview: a dry run that starts on a wall-clock poll and counts its
// tokens a frame at a time with little work in each, so the page can look quiet before the dry run ends. The step
// waits for its prompt, the last thing that reaches the host, so every count lands in the step.
async function watchPromptPreview() {
    const { eventSource, event_types } = await import('/scripts/events.js');
    globalThis.__perfPromptPreview = new Promise(resolve => {
        const prepared = (data, dryRun) => {
            if (!dryRun) return;
            eventSource.removeListener(event_types.GENERATE_AFTER_DATA, prepared);
            resolve();
        };
        eventSource.on(event_types.GENERATE_AFTER_DATA, prepared);
    });
}

async function promptPreview(page) {
    let timer;
    const timeout = new Promise((resolve, reject) => {
        timer = setTimeout(() => reject(new Error('The prompt preview did not finish within 10 s')), 10_000);
    });
    try {
        await Promise.race([page.evaluate(() => globalThis.__perfPromptPreview), timeout]);
    } finally {
        clearTimeout(timer);
    }
}
