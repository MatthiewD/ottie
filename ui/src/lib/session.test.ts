import type { Job } from '@ottie/api-types/Job';
import type { Settings } from '@ottie/api-types/Settings';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Api } from './api';
import { Session } from './session.svelte';
import { job } from './testing';

/** An events socket the test opens and closes by hand. */
class FakeSocket {
	onopen: (() => void) | null = null;
	onmessage: ((message: MessageEvent<string>) => void) | null = null;
	onclose: (() => void) | null = null;

	close(): void {
		this.onclose?.();
	}
}

/** A promise the test settles when it wants. */
function held<T>(): { promise: Promise<T>; resolve: (value: T) => void } {
	let resolve: (value: T) => void = () => {};
	const promise = new Promise<T>((settle) => (resolve = settle));
	return { promise, resolve };
}

let sockets: FakeSocket[];

/** The daemon behind every request but `/jobs`, which each test answers. */
function daemon(listJobs: () => Promise<Job[]>): void {
	sockets = [];
	vi.spyOn(Api.prototype, 'events').mockImplementation(() => {
		const socket = new FakeSocket();
		sockets.push(socket);
		return socket as unknown as WebSocket;
	});
	vi.spyOn(Api.prototype, 'listJobs').mockImplementation(listJobs);
	vi.spyOn(Api.prototype, 'listCategories').mockResolvedValue([]);
	vi.spyOn(Api.prototype, 'getSettings').mockResolvedValue({} as Settings);
	vi.spyOn(Api.prototype, 'listViews').mockResolvedValue([]);
	vi.spyOn(Api.prototype, 'listSlots').mockResolvedValue({ timeZone: null, slots: [] });
	vi.spyOn(Api.prototype, 'speedHistory').mockResolvedValue({ samples: [] });
	vi.spyOn(Api.prototype, 'getAccess').mockResolvedValue({
		device: null,
		readOnly: false,
		folders: { config: [], chosen: [] }
	});
}

/** Lets pending promises and the timers due by then run. */
async function settle(ms = 0): Promise<void> {
	await vi.advanceTimersByTimeAsync(ms);
}

describe('Session', () => {
	beforeEach(() => {
		vi.useFakeTimers();
	});

	afterEach(() => {
		vi.useRealTimers();
		vi.restoreAllMocks();
	});

	it('fetches the list again when the socket reconnected while it came', async () => {
		const first = held<Job[]>();
		const completed = [job({ state: 'completed' })];
		const listJobs = vi.fn().mockReturnValueOnce(first.promise).mockResolvedValue(completed);
		daemon(listJobs);
		const session = new Session('token', () => {});
		session.start();

		sockets[0].onopen?.();
		// The socket drops before the list comes, and the download finishes
		// meanwhile: the next socket never hears of it.
		sockets[0].close();
		await settle(500);
		sockets[1].onopen?.();
		first.resolve([job({ state: 'paused' })]);
		await settle();

		expect(listJobs).toHaveBeenCalledTimes(2);
		expect(session.list.map((shown) => shown.state)).toEqual(['completed']);
		expect(session.connection).toBe('live');
		session.stop();
	});

	it('shows no list as live once its socket has closed', async () => {
		const first = held<Job[]>();
		daemon(() => first.promise);
		const session = new Session('token', () => {});
		session.start();

		sockets[0].onopen?.();
		sockets[0].close();
		first.resolve([job()]);
		await settle();

		expect(session.connection).toBe('offline');
		session.stop();
	});

	it('tries a failed snapshot again, connecting until one comes', async () => {
		daemon(() => Promise.resolve([job()]));
		// One request fails once (the daemon restarting, say).
		vi.spyOn(Api.prototype, 'getAccess').mockRejectedValueOnce(new TypeError('fetch failed'));
		const session = new Session('token', () => {});
		session.start();

		sockets[0].onopen?.();
		await settle();
		expect(session.list).toEqual([]);
		expect(session.connection).toBe('connecting');
		expect(session.error).not.toBeNull();

		await settle(1000);
		expect(session.list).toHaveLength(1);
		expect(session.connection).toBe('live');
		session.stop();
	});

	it('forgets a snapshot that comes after it stopped', async () => {
		const first = held<Job[]>();
		daemon(() => first.promise);
		const session = new Session('token', () => {});
		session.start();

		sockets[0].onopen?.();
		session.stop();
		first.resolve([job()]);
		await settle();

		expect(session.connection).not.toBe('live');
		expect(session.list).toEqual([]);
	});
});
