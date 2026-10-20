import { describe, expect, it } from 'vitest';
import { withRequestId } from './daemon';

describe('withRequestId', () => {
	it('gives each download a random ID that Ottie accepts', () => {
		const job = { url: 'https://example.com/a.zip' };
		const first = withRequestId(job);
		const second = withRequestId(job);
		expect(first.requestId).toMatch(/^[A-Za-z0-9_-]{1,64}$/);
		expect(second.requestId).not.toBe(first.requestId);
		expect(first.url).toBe(job.url);
	});

	it('keeps the ID through every try', () => {
		const job = withRequestId({ url: 'https://example.com/a.zip' });
		expect(withRequestId(job)).toBe(job);
	});
});
