// Ottie on other computers (docs/decisions/0053): the switcher's rows, and
// the list a browser keeps. In the desktop app, the app keeps the list and
// the tokens (in the system's keychain) and shows one in its window; in a
// browser, each address keeps its own list here, and switching goes to the
// other one's own page, which remembers its own token.

import { m } from './paraglide/messages.js';
import { getLocale, type Locale } from './paraglide/runtime.js';

/** Another computer's Ottie, as the desktop app lists it. */
export interface Other {
	id: number;
	name: string;
	/** Its root: `https://nas.example.ts.net/`. */
	address: string;
}

/** The desktop app's list, and which one its window shows (null: this
 * computer's). */
export interface Daemons {
	shown: number | null;
	others: Other[];
}

/** One in a browser's list. Its token is kept only until it's first opened
 * (that page remembers it from then on). */
export interface Stored extends Other {
	token?: string;
}

/** Why one couldn't be added or shown: the desktop app's codes, and `here`
 * (a browser's own address). */
export type OthersRefusal =
	| 'badLink'
	| 'unreachable'
	| 'notOttie'
	| 'refused'
	| 'keychain'
	| 'notSaved'
	| 'notFound'
	| 'cantStart'
	| 'here';

/** A row of the switcher and of Settings. */
export interface Entry {
	/** null: this computer's in the app, this page's own in a browser. */
	id: number | null;
	name: string;
	address: string;
	/** The one shown now. */
	current: boolean;
}

/** As long as the desktop app allows. */
export const MAX_NAME = 60;
const MAX_TOKEN = 256;
const KEY = 'ottie.others';

/** A link's root and its token (`#token=…`, as Settings and `ottie device
 * add` give them); the token is null for a bare address. Null if it isn't
 * an http(s) address. */
export function parseLink(text: string): { address: string; token: string | null } | null {
	let url: URL;
	try {
		url = new URL(text.trim());
	} catch {
		return null;
	}
	if ((url.protocol !== 'http:' && url.protocol !== 'https:') || url.host === '') {
		return null;
	}
	const fragment = /^#token=(.*)$/.exec(url.hash);
	let token: string | null = null;
	if (fragment) {
		token = fragment[1];
		if (token.length === 0 || token.length > MAX_TOKEN || !/^[\w.~-]+$/.test(token)) {
			return null;
		}
	}
	return { address: `${url.origin}/`, token };
}

/** The name given, tidied, else the address's host. */
export function nameFor(name: string, address: string): string {
	const tidy = [...name.replace(/[\u0000-\u001f\u007f-\u009f]/g, '').trim()]
		.slice(0, MAX_NAME)
		.join('');
	return tidy || new URL(address).host;
}

/** The desktop app's rows: this computer's first, then the list. */
export function appEntries(daemons: Daemons, local: string, thisComputer: string): Entry[] {
	return [
		{ id: null, name: thisComputer, address: `${local}/`, current: daemons.shown === null },
		...daemons.others.map((other) => ({ ...other, current: daemons.shown === other.id }))
	];
}

/** A browser's rows: this page's own Ottie first (by its host), then the list. */
export function browserEntries(stored: Stored[], here: string): Entry[] {
	return [
		{ id: null, name: new URL(here).host, address: `${here}/`, current: true },
		...stored.map(({ id, name, address }) => ({ id, name, address, current: false }))
	];
}

/** A browser's list with one more, from a link or an address; or why not. */
export function withOther(
	stored: Stored[],
	name: string,
	link: string,
	here: string
): Stored[] | OthersRefusal {
	const parsed = parseLink(link);
	if (!parsed) {
		return 'badLink';
	}
	if (parsed.address === `${here}/`) {
		return 'here';
	}
	const id = Math.max(0, ...stored.map((other) => other.id)) + 1;
	const added: Stored = { id, name: nameFor(name, parsed.address), address: parsed.address };
	if (parsed.token) {
		added.token = parsed.token;
	}
	return [...stored, added];
}

/** Where opening one goes: its page, signed in if its token is still kept. */
export function openingLink(other: Stored): string {
	return other.token ? `${other.address}#token=${other.token}` : other.address;
}

/** Opening `id` from the list: where to go, and the list to keep, its token
 * forgotten (its page remembers it from now on). null if it isn't listed. */
export function opening(stored: Stored[], id: number): { link: string; kept: Stored[] } | null {
	const other = stored.find((each) => each.id === id);
	if (!other) {
		return null;
	}
	const kept = stored.map((each) =>
		each.id === id ? { id: each.id, name: each.name, address: each.address } : each
	);
	return { link: openingLink(other), kept };
}

/** A browser's list, from this page's storage. */
export function loadOthers(): Stored[] {
	try {
		const saved: unknown = JSON.parse(localStorage.getItem(KEY) ?? '[]');
		return Array.isArray(saved) ? saved.filter(isStored) : [];
	} catch {
		return []; // Storage blocked or spoiled: an empty list.
	}
}

export function saveOthers(stored: Stored[]): void {
	try {
		localStorage.setItem(KEY, JSON.stringify(stored));
	} catch {
		// Storage blocked: it lasts until the page is closed.
	}
}

function isStored(value: unknown): value is Stored {
	if (typeof value !== 'object' || value === null) {
		return false;
	}
	const { id, name, address, token } = value as Record<string, unknown>;
	return (
		typeof id === 'number' &&
		typeof name === 'string' &&
		typeof address === 'string' &&
		parseLink(address) !== null &&
		(token === undefined || typeof token === 'string')
	);
}

/** Why it couldn't be added or shown, in words. */
export function explainOthers(refusal: OthersRefusal, locale: Locale = getLocale()): string {
	const options = { locale };
	switch (refusal) {
		case 'badLink':
			return m.others_bad_link({}, options);
		case 'unreachable':
			return m.others_unreachable({}, options);
		case 'notOttie':
			return m.others_not_ottie({}, options);
		case 'refused':
			return m.others_refused({}, options);
		case 'keychain':
			return m.others_keychain({}, options);
		case 'notSaved':
			return m.others_not_saved({}, options);
		case 'notFound':
			return m.others_not_found({}, options);
		case 'cantStart':
			return m.others_cant_start({}, options);
		case 'here':
			return m.others_here({}, options);
	}
}

/** A refusal from the desktop app; an unknown one (a newer app) counts as
 * unreachable. */
export function asRefusal(why: unknown): OthersRefusal {
	const known: OthersRefusal[] = [
		'badLink',
		'unreachable',
		'notOttie',
		'refused',
		'keychain',
		'notSaved',
		'notFound',
		'cantStart'
	];
	return known.find((code) => code === why) ?? 'unreachable';
}
