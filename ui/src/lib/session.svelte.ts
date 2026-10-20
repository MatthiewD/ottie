// What the downloads page shows, kept live: the job list from `GET /jobs`,
// updated by the events WebSocket, which reconnects on its own.

import type { AccessInfo } from '@ottie/api-types/AccessInfo';
import type { AddJob } from '@ottie/api-types/AddJob';
import type { AddedDevice } from '@ottie/api-types/AddedDevice';
import type { AddedJobs } from '@ottie/api-types/AddedJobs';
import type { CategoryBody } from '@ottie/api-types/CategoryBody';
import type { Device } from '@ottie/api-types/Device';
import type { JobChange } from '@ottie/api-types/JobChange';
import type { Matched } from '@ottie/api-types/Matched';
import type { Preview } from '@ottie/api-types/Preview';
import type { Reorder } from '@ottie/api-types/Reorder';
import type { Settings } from '@ottie/api-types/Settings';
import type { SettingsChange } from '@ottie/api-types/SettingsChange';
import type { SetupStep } from '@ottie/api-types/SetupStep';
import type { Event as ApiEvent } from '@ottie/api-types/Event';
import type { SlotBody } from '@ottie/api-types/SlotBody';
import type { TrustedCertificate } from '@ottie/api-types/TrustedCertificate';
import type { ViewBody } from '@ottie/api-types/ViewBody';
import type { Looked } from './adding';
import { Api, ApiFailure, Unauthorized } from './api';
import type { Action } from './bulk';
import { GENERAL, applyCategoryEvent, sorted, type Categories } from './categories';
import { explainRefusal } from './explain';
import { m } from './paraglide/messages.js';
import { applyEvent, newestFirst, nextRate, type Jobs, type Rate } from './jobs';
import { applySlotEvent, slotNames, sortedSlots, type Slots } from './slots';
import { addSample, nextSample, totalSpeed } from './speed';
import { applyViewEvent, sortedViews, type Views } from './views';

export type Connection = 'connecting' | 'live' | 'offline';

/** The daemon's answer, or why there is none (in the page's language; null
 * once signed out). */
export type Answer<T> = { ok: true; value: T } | { ok: false; why: string | null };

const FIRST_RETRY_MS = 500;
const LONGEST_RETRY_MS = 10_000;
/** A snapshot that failed is fetched again after this, doubled each time up
 * to `LONGEST_RETRY_MS`. */
const FIRST_SNAPSHOT_RETRY_MS = 1000;

export class Session {
	jobs: Jobs = $state({});
	rates: Record<string, Rate> = $state({});
	connection: Connection = $state('connecting');
	/** The last thing that went wrong, for a message on the page. */
	error: string | null = $state(null);
	/** Updated every second, so "speed" and "retrying in" stay current. */
	now = $state(Date.now());
	/** Total speed, one sample per second, oldest first: the daemon's last
	 * hour, then the page's own (docs/decisions/0044). */
	history: number[] = $state.raw([]);
	list = $derived(newestFirst(this.jobs));
	categories: Categories = $state({});
	/** General first, then by name. */
	categoryList = $derived(sorted(this.categories));
	/** Null until the daemon has answered. */
	settings: Settings | null = $state(null);
	views: Views = $state({});
	/** Oldest first. */
	viewList = $derived(sortedViews(this.views));
	slots: Slots = $state({});
	/** In the order of the list: the higher one wins. */
	slotList = $derived(sortedSlots(this.slots));
	/** Each slot's name (or when it is), for "Held by …". */
	slotNames = $derived(slotNames(this.slotList));
	/** The time zone the slots follow (the daemon's), if it has a name. */
	timeZone: string | null = $state(null);
	/** The setup, run again from Settings: in this window only, and nothing
	 * about it is saved but the settings it changes (docs/decisions/0050). */
	settingUpAgain = $state(false);
	/** Who this page's token belongs to (docs/decisions/0052); null until known. */
	access: AccessInfo | null = $state(null);
	/** A device that may only look: no adding, pausing, changing. */
	readOnly = $derived(this.access?.readOnly ?? false);
	/** The computer running Ottie (its token file's token): it alone manages
	 * devices and the folders they may use. */
	owner = $derived(this.access !== null && this.access.device === null);

	private readonly api: Api;
	private socket: WebSocket | null = null;
	/** Events that arrive while the list is being fetched: applied after it. */
	private pending: ApiEvent[] | null = null;
	private retryMs = FIRST_RETRY_MS;
	private snapshotRetryMs = FIRST_SNAPSHOT_RETRY_MS;
	/** The clock, and a reconnect or a snapshot waiting to happen, cleared on `stop`. */
	private timers = new Set<ReturnType<typeof setTimeout>>();
	private stopped = false;
	/** Counts the sockets opened and closed (and `stop`): a snapshot started
	 * under an earlier one may miss what changed since, so it's fetched again. */
	private generation = 0;
	/** The events socket is open: what a snapshot shows is then kept live. */
	private open = false;
	/** When the daemon's speed history last came: the page's own rates take
	 * a moment to measure after that (see `nextSample`). */
	private historyAt = 0;

	constructor(
		token: string,
		private readonly onUnauthorized: () => void
	) {
		this.api = new Api(token);
	}

	start(): void {
		this.connect();
		const clock = setInterval(() => {
			this.now = Date.now();
			const own = totalSpeed(this.rates, this.now);
			this.history = addSample(
				this.history,
				nextSample(this.history, own, this.now - this.historyAt)
			);
		}, 1000);
		this.timers.add(clock);
	}

	stop(): void {
		this.stopped = true;
		this.generation += 1;
		this.socket?.close();
		this.timers.forEach((timer) => clearTimeout(timer));
	}

	async add(job: AddJob): Promise<boolean> {
		return this.attempt(() => this.api.addJob(job));
	}

	/** Adds the jobs that can be, in order (docs/decisions/0036). */
	addMany(jobs: AddJob[]): Promise<Answer<AddedJobs>> {
		return this.answer(() => this.api.addJobs(jobs));
	}

	/** The links in `text`, and what adding each would do. */
	preview(text: string, destDir?: string): Promise<Answer<Preview>> {
		return this.answer(() => this.api.previewLinks(text, destDir));
	}

	/** What adding the one link `url` (saved in `destDir`, if given) would
	 * do: its category, its name and whether that file is already there;
	 * null if the daemon can't tell (adding it will say why). */
	async look(url: string, destDir?: string): Promise<Looked | null> {
		try {
			const { links } = await this.api.previewLinks(url, destDir);
			const [found] = links;
			return links.length === 1 && found.kind === 'link'
				? { matched: found.matched, name: found.name, exists: found.exists }
				: null;
		} catch {
			return null;
		}
	}

	/** The category a download of `url` (saved in `destDir`, if given)
	 * would get; null if the daemon can't tell (adding it will say why). */
	async match(url: string, destDir?: string): Promise<Matched | null> {
		try {
			return await this.api.matchCategory(url, destDir);
		} catch {
			return null;
		}
	}

	/** Does it to every job in `ids` at once; any failure shows on the page. */
	async run(action: Action, ids: string[]): Promise<void> {
		const request = {
			resume: (id: string) => this.api.resume(id),
			pause: (id: string) => this.api.pause(id),
			cancel: (id: string) => this.api.cancel(id),
			remove: (id: string) => this.api.remove(id)
		}[action];
		await Promise.all(ids.map((id) => this.attempt(() => request(id))));
	}

	/** Moves the jobs in the queue; every page hears of it through `changed`
	 * events. A failure shows on the page. */
	async reorder(ids: string[], to: Reorder): Promise<void> {
		await this.attempt(() => this.api.reorder(ids, to));
	}

	/** The job's play link as a full address; null if the daemon can't give one. */
	async streamLink(id: string): Promise<string | null> {
		try {
			const { url } = await this.api.streamLink(id);
			return new URL(url, location.href).href;
		} catch {
			return null;
		}
	}

	/** Moves the jobs to `category`, as the user's choice. */
	async move(ids: string[], category: number): Promise<void> {
		await Promise.all(ids.map((id) => this.attempt(() => this.api.setCategory(id, category))));
	}

	/** Changes one download's own speed limit or priority; every page hears
	 * of it through a `changed` event. Resolves to null once done, else to
	 * why not. */
	async changeJob(id: string, change: JobChange): Promise<string | null> {
		try {
			await this.api.changeJob(id, change);
			return null;
		} catch (error) {
			return this.why(error);
		}
	}

	/** Adds a category (`id` null) or replaces one's settings. Resolves to
	 * null when saved, else to why not (in the page's language). */
	async saveCategory(id: number | null, body: CategoryBody): Promise<string | null> {
		try {
			await (id === null ? this.api.addCategory(body) : this.api.updateCategory(id, body));
			return null;
		} catch (error) {
			return this.why(error);
		}
	}

	removeCategory(id: number): Promise<boolean> {
		return this.attempt(() => this.api.removeCategory(id));
	}

	/** Saves a new view (`id` null) or replaces one; every page hears of it
	 * through a `viewChanged` event. Resolves to null once saved, else to
	 * why not. */
	async saveView(id: number | null, body: ViewBody): Promise<string | null> {
		try {
			await (id === null ? this.api.addView(body) : this.api.updateView(id, body));
			return null;
		} catch (error) {
			return this.why(error);
		}
	}

	removeView(id: number): Promise<boolean> {
		return this.attempt(() => this.api.removeView(id));
	}

	/** Adds a time slot (`id` null) or replaces one; every page hears of it
	 * through a `slotChanged` event. Resolves to null once saved, else to
	 * why not. */
	async saveSlot(id: number | null, body: SlotBody): Promise<string | null> {
		try {
			await (id === null ? this.api.addSlot(body) : this.api.updateSlot(id, body));
			return null;
		} catch (error) {
			return this.why(error);
		}
	}

	removeSlot(id: number): Promise<boolean> {
		return this.attempt(() => this.api.removeSlot(id));
	}

	/** Every slot's ID in the new order; a failure shows on the page. */
	async reorderSlots(ids: number[]): Promise<void> {
		await this.attempt(() => this.api.reorderSlots(ids));
	}

	/** Changes the daemon's settings; every page (here and on other devices)
	 * hears of it through the `settingsChanged` event. Resolves to null
	 * once done, else to why not. */
	async changeSettings(change: SettingsChange): Promise<string | null> {
		try {
			await this.api.changeSettings(change);
			return null;
		} catch (error) {
			return this.why(error);
		}
	}

	/** Puts General in `folder`, the downloads folder, unless it's there
	 * already. Resolves to null once done, else to why not. */
	async setDownloadsFolder(folder: string): Promise<string | null> {
		const general = this.categories[GENERAL];
		if (!general || folder === general.folder) {
			return null;
		}
		const { name, extensions, speedLimit, maxConcurrent } = general;
		return this.saveCategory(GENERAL, { name, folder, extensions, speedLimit, maxConcurrent });
	}

	/** Creates the categories, in order. Resolves to null once done, else to
	 * why not; what was created stays. */
	async addCategories(bodies: CategoryBody[]): Promise<string | null> {
		for (const body of bodies) {
			const why = await this.saveCategory(null, body);
			if (why !== null) {
				return why;
			}
		}
		return null;
	}

	/** Goes past a step of the setup, with the settings it changes: saved
	 * for every front end, so a setup stopped halfway carries on after it;
	 * the setup run again from Settings only changes the settings. */
	passStep(step: SetupStep, change: SettingsChange = {}): Promise<string | null> {
		if (this.settingUpAgain) {
			return Object.keys(change).length ? this.changeSettings(change) : Promise.resolve(null);
		}
		return this.changeSettings({ ...change, setupStep: step });
	}

	/** Ends the setup: done for every front end (it isn't offered again),
	 * or the one run again in this window. */
	async endSetup(): Promise<string | null> {
		if (this.settingUpAgain) {
			this.settingUpAgain = false;
			return null;
		}
		const why = await this.changeSettings({ setupDone: true, setupStep: null });
		if (why === null && this.settings) {
			this.settings.setupDone = true;
		}
		return why;
	}

	/** Every device with a token of its own, or why not. */
	devices(): Promise<Answer<Device[]>> {
		return this.answer(() => this.api.listDevices());
	}

	/** A token for another device, shown this once. */
	addDevice(name: string, readOnly: boolean): Promise<Answer<AddedDevice>> {
		return this.answer(() => this.api.addDevice(name, readOnly));
	}

	removeDevice(id: number): Promise<boolean> {
		return this.attempt(() => this.api.removeDevice(id));
	}

	/** FTPS servers' certificates trusted by hand, or why not. */
	certificates(): Promise<Answer<TrustedCertificate[]>> {
		return this.answer(() => this.api.listCertificates());
	}

	/** Trusts the certificate a download stopped at, then tries it again
	 * (docs/decisions/0054). */
	async trustAndRetry(id: string, host: string, fingerprint: string): Promise<void> {
		if (await this.attempt(() => this.api.trustCertificate(host, fingerprint))) {
			await this.attempt(() => this.api.resume(id));
		}
	}

	distrustCertificate(id: number): Promise<boolean> {
		return this.attempt(() => this.api.distrustCertificate(id));
	}

	/** The folders chosen in Settings that devices may use. Resolves to
	 * null once saved, else to why not. */
	async setFolders(folders: string[]): Promise<string | null> {
		try {
			const allowed = await this.api.setFolders(folders);
			if (this.access) {
				this.access.folders = allowed;
			}
			return null;
		} catch (error) {
			return this.why(error);
		}
	}

	/** Runs a request; on failure shows why and returns false. */
	private async attempt(request: () => Promise<unknown>): Promise<boolean> {
		try {
			await request();
			this.error = null;
			return true;
		} catch (error) {
			this.fail(error);
			return false;
		}
	}

	private fail(error: unknown): void {
		const why = this.why(error);
		if (why !== null) {
			this.error = why;
		}
	}

	/** What went wrong, for the page; null after signing out (a refused token). */
	private async answer<T>(request: () => Promise<T>): Promise<Answer<T>> {
		try {
			return { ok: true, value: await request() };
		} catch (error) {
			return { ok: false, why: this.why(error) };
		}
	}

	private why(error: unknown): string | null {
		if (error instanceof Unauthorized) {
			this.stop();
			this.onUnauthorized();
			return null;
		}
		if (error instanceof ApiFailure) {
			return explainRefusal(error.body, error.message);
		}
		// fetch() failed: the daemon is stopped or unreachable.
		return m.connection_offline();
	}

	private connect(): void {
		if (this.stopped) {
			return;
		}
		this.connection = 'connecting';
		let opened = false;
		const socket = this.api.events();
		this.socket = socket;
		socket.onopen = () => {
			opened = true;
			this.open = true;
			this.retryMs = FIRST_RETRY_MS;
			this.generation += 1;
			if (this.pending) {
				// A snapshot is on its way, but from before this socket: it's
				// fetched again. What the socket before sent is in the new one,
				// and played after it, could bring an old state back.
				this.pending = [];
			}
			void this.resync();
		};
		socket.onmessage = (message: MessageEvent<string>) => {
			this.receive(JSON.parse(message.data) as ApiEvent);
		};
		socket.onclose = () => {
			this.open = false;
			this.generation += 1;
			if (this.stopped) {
				return;
			}
			this.connection = 'offline';
			// A refused token closes the socket without saying why; a plain
			// request tells a wrong token from a stopped daemon.
			if (!opened) {
				this.api.listJobs().catch((error: unknown) => {
					if (error instanceof Unauthorized) {
						this.fail(error);
					}
				});
			}
			const retry = setTimeout(() => {
				this.timers.delete(retry);
				this.connect();
			}, this.retryMs);
			this.timers.add(retry);
			this.retryMs = Math.min(this.retryMs * 2, LONGEST_RETRY_MS);
		};
	}

	private receive(event: ApiEvent): void {
		if (this.pending) {
			this.pending.push(event);
			return;
		}
		if (event.type === 'progress') {
			this.rates[event.id] = nextRate(this.rates[event.id], event.downloaded, Date.now());
		} else if (event.type === 'removed') {
			delete this.rates[event.id];
		} else if (event.type === 'settingsChanged') {
			this.settings = event.settings;
		}
		applyCategoryEvent(this.categories, event);
		applyViewEvent(this.views, event);
		applySlotEvent(this.slots, event);
		if (applyEvent(this.jobs, event)) {
			void this.resync();
		}
	}

	/** Fetches the whole list, then replays what arrived meanwhile. Events are
	 * full states in order, so the replay always ends on the latest state.
	 * A snapshot started before the socket reconnected is fetched again; one
	 * that fails is tried again a moment later, and the page says it's
	 * connecting until one comes. */
	private async resync(): Promise<void> {
		if (this.pending) {
			return; // Already fetching.
		}
		this.pending = [];
		try {
			let generation: number;
			let snapshot: Snapshot;
			do {
				generation = this.generation;
				snapshot = await this.snapshot();
			} while (generation !== this.generation && this.open && !this.stopped);
			if (generation !== this.generation || this.stopped) {
				return; // Closed meanwhile: the next socket's open fetches one.
			}
			const { jobs, categories, settings, views, slots, speed, access } = snapshot;
			this.access = access;
			this.history = speed.samples;
			this.historyAt = Date.now();
			this.jobs = Object.fromEntries(jobs.map((job) => [job.id, job]));
			this.categories = Object.fromEntries(categories.map((category) => [category.id, category]));
			this.settings = settings;
			this.views = Object.fromEntries(views.map((view) => [view.id, view]));
			this.slots = Object.fromEntries(slots.slots.map((slot) => [slot.id, slot]));
			this.timeZone = slots.timeZone;
			this.connection = 'live';
			this.snapshotRetryMs = FIRST_SNAPSHOT_RETRY_MS;
		} catch (error) {
			this.fail(error);
			this.retrySnapshot();
		} finally {
			const missed = this.pending ?? [];
			this.pending = null;
			missed.forEach((event) => this.receive(event));
		}
	}

	private async snapshot(): Promise<Snapshot> {
		const [jobs, categories, settings, views, slots, speed, access] = await Promise.all([
			this.api.listJobs(),
			this.api.listCategories(),
			this.api.getSettings(),
			this.api.listViews(),
			this.api.listSlots(),
			this.api.speedHistory(),
			this.api.getAccess()
		]);
		return { jobs, categories, settings, views, slots, speed, access };
	}

	/** After a failed snapshot: another a moment later, while the same socket
	 * is open (a reconnect fetches one anyway). Signed out: none. */
	private retrySnapshot(): void {
		if (this.stopped || !this.open) {
			return;
		}
		this.connection = 'connecting';
		const generation = this.generation;
		const retry = setTimeout(() => {
			this.timers.delete(retry);
			if (this.generation === generation) {
				void this.resync();
			}
		}, this.snapshotRetryMs);
		this.timers.add(retry);
		this.snapshotRetryMs = Math.min(this.snapshotRetryMs * 2, LONGEST_RETRY_MS);
	}
}

/** Everything `resync` fetches, at once. */
type Snapshot = {
	jobs: Awaited<ReturnType<Api['listJobs']>>;
	categories: Awaited<ReturnType<Api['listCategories']>>;
	settings: Settings;
	views: Awaited<ReturnType<Api['listViews']>>;
	slots: Awaited<ReturnType<Api['listSlots']>>;
	speed: Awaited<ReturnType<Api['speedHistory']>>;
	access: AccessInfo;
};
