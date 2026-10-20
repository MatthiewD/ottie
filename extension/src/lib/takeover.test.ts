import { describe, expect, it } from 'vitest';
import { DEFAULTS, type ExtensionSettings } from './settings';
import { settle, skipReason, type BrowserDownload, type BrowserDownloads } from './takeover';

const SIGNED_IN: ExtensionSettings = {
	...DEFAULTS,
	pairing: { address: 'http://127.0.0.1:24068', token: 'abc' }
};

const ITEM: BrowserDownload = {
	url: 'https://example.com/files/a.zip',
	incognito: false,
	state: 'in_progress',
	totalBytes: 5_000_000
};

const never = () => false;

describe('skipReason', () => {
	it('takes an ordinary download', () => {
		expect(skipReason(ITEM, SIGNED_IN, never)).toBeNull();
	});

	it('takes one of unknown size, whatever the smallest size', () => {
		const settings = { ...SIGNED_IN, minSize: 1024 };
		expect(skipReason({ ...ITEM, totalBytes: 0 }, settings, never)).toBeNull();
		expect(skipReason({ ...ITEM, totalBytes: -1 }, settings, never)).toBeNull();
	});

	it('leaves the rest to the browser, saying why', () => {
		const cases: [Partial<BrowserDownload>, Partial<ExtensionSettings>, string][] = [
			[{}, { takeOver: false }, 'off'],
			[{}, { pairing: null }, 'signedOut'],
			[{ incognito: true }, {}, 'private'],
			[{ url: 'blob:https://example.com/1234' }, {}, 'scheme'],
			[{ url: 'data:text/plain,hi' }, {}, 'scheme'],
			[{ url: 'not a link' }, {}, 'scheme'],
			[{ byExtensionId: 'other' }, {}, 'extension'],
			[{ state: 'complete' }, {}, 'notRunning'],
			[{ url: 'http://127.0.0.1:24068/api/v1/stream/x/a.mp4' }, {}, 'ottie'],
			[{ totalBytes: 1000 }, { minSize: 1024 }, 'small'],
			[{ url: 'https://dl.example.com/a.zip' }, { excluded: ['example.com'] }, 'excluded']
		];
		for (const [item, settings, why] of cases) {
			expect(skipReason({ ...ITEM, ...item }, { ...SIGNED_IN, ...settings }, never)).toBe(why);
		}
	});

	it('leaves a download that came from a form to the browser', () => {
		const posted = (url: string) => url === ITEM.url;
		expect(skipReason(ITEM, SIGNED_IN, posted)).toBe('posted');
	});
});

/** A browser whose download ends up in `state`; `refuses`: cancelling
 * fails, as Firefox's does for a finished download. */
function browser(state: string | undefined, refuses = false) {
	const calls: string[] = [];
	const downloads: BrowserDownloads = {
		cancel: async () => {
			calls.push('cancel');
			if (refuses) {
				throw new Error('Download is not active');
			}
		},
		state: async () => {
			calls.push('state');
			return state;
		},
		erase: async () => {
			calls.push('erase');
		}
	};
	return { downloads, calls };
}

describe('settle', () => {
	const removed = () => {
		const calls: string[] = [];
		return { calls, remove: async () => void calls.push('remove') };
	};

	it("takes the browser's download off its list once Ottie has it", async () => {
		const { downloads, calls } = browser('interrupted');
		const ottie = removed();
		expect(await settle(downloads, 7, ottie.remove)).toBe('ottie');
		expect(calls).toEqual(['cancel', 'state', 'erase']);
		expect(ottie.calls).toEqual([]);
	});

	it("removes Ottie's job when the browser finished first", async () => {
		const { downloads, calls } = browser('complete');
		const ottie = removed();
		expect(await settle(downloads, 7, ottie.remove)).toBe('browser');
		expect(calls).toEqual(['cancel', 'state']);
		expect(ottie.calls).toEqual(['remove']);
	});

	it('does so too when cancelling a finished download fails (Firefox)', async () => {
		const { downloads } = browser('complete', true);
		const ottie = removed();
		expect(await settle(downloads, 7, ottie.remove)).toBe('browser');
		expect(ottie.calls).toEqual(['remove']);
	});

	it('says when both are left', async () => {
		const ottie = removed();
		expect(await settle(browser('in_progress', true).downloads, 7, ottie.remove)).toBe('both');
		let tries = 0;
		const failing = async () => {
			tries += 1;
			throw new Error('unreachable');
		};
		const waits: number[] = [];
		const wait = async (ms: number) => void waits.push(ms);
		expect(await settle(browser('complete').downloads, 7, failing, wait)).toBe('both');
		expect(tries).toBe(3);
		expect(waits).toEqual([1000, 2000]);
		expect(ottie.calls).toEqual([]);
	});

	it("asks again when Ottie doesn't answer the removal at once", async () => {
		let tries = 0;
		const once = async () => {
			tries += 1;
			if (tries === 1) {
				throw new Error('unreachable');
			}
		};
		const waits: number[] = [];
		const wait = async (ms: number) => void waits.push(ms);
		expect(await settle(browser('complete').downloads, 7, once, wait)).toBe('browser');
		expect(tries).toBe(2);
		expect(waits).toEqual([1000]);
	});

	it('leaves Ottie the download the browser no longer lists', async () => {
		const { downloads, calls } = browser(undefined);
		expect(await settle(downloads, 7, removed().remove)).toBe('ottie');
		expect(calls).toEqual(['cancel', 'state', 'erase']);
	});
});
