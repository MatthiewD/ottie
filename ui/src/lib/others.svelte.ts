// Ottie on other computers, live (docs/decisions/0053): the desktop app's
// list (it switches its window), or this browser's for this address (it
// goes to the other page). For the title bar's switcher and Settings.

import { m } from './paraglide/messages.js';
import {
	appEntries,
	browserEntries,
	explainOthers,
	loadOthers,
	opening,
	saveOthers,
	withOther,
	type Daemons,
	type Entry,
	type Stored
} from './others.js';
import {
	addDaemon,
	daemons,
	followDaemons,
	inApp,
	localAddress,
	removeDaemon,
	switchDaemon
} from './window.js';

/** The list, shared by every part of the page that shows it (the title
 * bar and Settings). A browser's is changed here, then saved: if storage
 * is blocked, it lasts as long as the page. */
const shared: { daemons: Daemons | null; stored: Stored[] } = $state({
	daemons: null,
	stored: []
});

export class LiveOthers {
	/** In the desktop app (its list), else in a browser (this address's). */
	readonly app = inApp();
	/** The rows: this computer's (or this page's) Ottie first. */
	entries: Entry[] = $derived.by(() => {
		if (!this.app) {
			return browserEntries(shared.stored, location.origin);
		}
		const local = localAddress();
		return shared.daemons && local
			? appEntries(shared.daemons, local, m.others_this_computer())
			: [];
	});
	/** Switching, adding or removing: wait. */
	busy = $state(false);

	/** Reads the list and follows its changes (another window, another
	 * tab). Returns a function that stops. */
	start(): () => void {
		if (this.app) {
			daemons().then(
				(answer) => (shared.daemons = answer),
				() => {}
			);
			return followDaemons((answer) => (shared.daemons = answer));
		}
		shared.stored = loadOthers();
		const reload = () => (shared.stored = loadOthers());
		window.addEventListener('storage', reload);
		return () => window.removeEventListener('storage', reload);
	}

	/** Adds one from a sign-in link (or, in a browser, an address). Resolves
	 * to why not, in words, or null. */
	async add(name: string, link: string): Promise<string | null> {
		return this.#busy(async () => {
			if (this.app) {
				const answer = await addDaemon(name, link);
				if (typeof answer === 'string') {
					return explainOthers(answer);
				}
				shared.daemons = answer;
				return null;
			}
			const next = withOther(shared.stored, name, link, location.origin);
			if (typeof next === 'string') {
				return explainOthers(next);
			}
			saveOthers(next);
			shared.stored = next;
			return null;
		});
	}

	async remove(id: number): Promise<string | null> {
		return this.#busy(async () => {
			if (this.app) {
				const answer = await removeDaemon(id);
				if (typeof answer === 'string') {
					return explainOthers(answer);
				}
				shared.daemons = answer;
				return null;
			}
			const next = shared.stored.filter((other) => other.id !== id);
			saveOthers(next);
			shared.stored = next;
			return null;
		});
	}

	/** Shows that one: the app's window moves to it, or this page goes
	 * there. Resolves to why not, in words, or null (then this page is
	 * going away). */
	async open(id: number | null): Promise<string | null> {
		return this.#busy(async () => {
			if (this.app) {
				const refusal = await switchDaemon(id);
				return refusal && explainOthers(refusal);
			}
			const going = id === null ? null : opening(shared.stored, id);
			if (!going) {
				return null;
			}
			saveOthers(going.kept);
			shared.stored = going.kept;
			location.assign(going.link);
			return null;
		});
	}

	async #busy(work: () => Promise<string | null>): Promise<string | null> {
		this.busy = true;
		try {
			return await work();
		} finally {
			this.busy = false;
		}
	}
}
