import { afterEach, describe, expect, it, vi } from 'vitest';
import { Refused, removeJob, withRequestId } from './daemon';

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

describe('removeJob', () => {
	const pairing = { address: 'http://127.0.0.1:24068', token: 't' };
	const answering = (status: number, body: unknown) =>
		vi.stubGlobal(
			'fetch',
			vi.fn(async () => new Response(JSON.stringify(body), { status }))
		);

	afterEach(() => {
		vi.unstubAllGlobals();
	});

	it('counts a download already gone as taken off', async () => {
		answering(404, { code: 'jobNotFound', id: 'x', error: 'no job with ID x' });
		await expect(removeJob(pairing, 'x')).resolves.toBeUndefined();
	});

	it('says when Ottie refused', async () => {
		answering(500, { code: 'storageFailed', error: 'disk full' });
		await expect(removeJob(pairing, 'x')).rejects.toBeInstanceOf(Refused);
	});
});
