// How the Downloads list is laid out: the sort order and grouping by status.
// Unlike the filters (in the address), it's remembered per device, so it
// sticks between visits.

import type { Arrangement } from '@ottie/api-types/Arrangement';
import type { Job } from '@ottie/api-types/Job';
import type { SortDirection } from '@ottie/api-types/SortDirection';
import type { SortKey } from '@ottie/api-types/SortKey';
import { GROUPS, groupOf, type Group } from './filters';
import { displayName, formatCount, formatSpeed } from './format';
import { byCreation } from './jobs';
import { m } from './paraglide/messages.js';
import { getLocale, type Locale } from './paraglide/runtime.js';
import { byQueue } from './queue';
import { fraction } from './status';

// The same types as a saved view's, so a view can hold an arrangement.
export type { Arrangement, SortKey };
export type Direction = SortDirection;

export const SORT_KEYS: SortKey[] = ['added', 'name', 'size', 'progress'];

export const DEFAULT_ARRANGEMENT: Arrangement = { sort: 'added', direction: 'asc', grouped: true };

/** The direction a key starts in when picked: oldest, largest and most done
 * first, names from A. */
export const FIRST_DIRECTION: Record<SortKey, Direction> = {
	added: 'asc',
	name: 'asc',
	size: 'desc',
	progress: 'desc'
};

/** A click on a column header: the same column turns the order around,
 * another one starts in its first direction. */
export function sortBy(arrangement: Arrangement, sort: SortKey): Arrangement {
	if (arrangement.sort === sort) {
		return { ...arrangement, direction: arrangement.direction === 'asc' ? 'desc' : 'asc' };
	}
	return { ...arrangement, sort, direction: FIRST_DIRECTION[sort] };
}

/** The size to sort by: unknown until the server says (a finished download
 * knows it). */
function size(job: Job): number | null {
	return job.total ?? (job.state === 'completed' ? job.downloaded : null);
}

export function sortJobs(
	jobs: Job[],
	sort: SortKey,
	direction: Direction,
	locale: Locale = getLocale()
): Job[] {
	const sign = direction === 'asc' ? 1 : -1;
	// Made once: `localeCompare` with options makes one for each comparison.
	const names = new Intl.Collator(locale, { numeric: true, sensitivity: 'base' });
	/** Unknown values go last either way; ties, newest first. */
	const numbers = (value: (job: Job) => number | null) => (a: Job, b: Job) => {
		const x = value(a);
		const y = value(b);
		if (x === null || y === null) {
			return x === y ? 0 : x === null ? 1 : -1;
		}
		return sign * (x - y);
	};
	const compare: (a: Job, b: Job) => number =
		sort === 'added'
			? (a, b) => sign * byCreation(a, b)
			: sort === 'name'
				? (a, b) => sign * names.compare(displayName(a), displayName(b))
				: numbers(sort === 'size' ? size : fraction);
	return [...jobs].sort((a, b) => compare(a, b) || byCreation(b, a));
}

/** A run of rows: one group, or the whole list (`group` null). */
export interface Section {
	group: Group | null;
	jobs: Job[];
}

/** The rows to show, in order. Empty groups are left out. The waiting group
 * is the queue: it's always in queue order, the next to start on top
 * (docs/decisions/0032). */
export function arrange(
	jobs: Job[],
	arrangement: Arrangement,
	locale: Locale = getLocale()
): Section[] {
	const sorted = sortJobs(jobs, arrangement.sort, arrangement.direction, locale);
	if (!arrangement.grouped) {
		return sorted.length === 0 ? [] : [{ group: null, jobs: sorted }];
	}
	return GROUPS.map((group) => {
		const jobs = sorted.filter((job) => groupOf(job) === group);
		return { group, jobs: group === 'waiting' ? jobs.sort(byQueue) : jobs };
	}).filter((section) => section.jobs.length > 0);
}

/** What a group's header says at its end: the total speed of the running
 * ones, how many of the download slots are taken above the waiting ones. */
export function groupNote(
	group: Group,
	facts: { speed: number; running: number; slots: number | null },
	locale: Locale = getLocale()
): string {
	if (group === 'running' && facts.speed > 0) {
		return formatSpeed(facts.speed, locale);
	}
	if (group === 'waiting' && facts.slots !== null) {
		return m.group_slots(
			{ used: formatCount(facts.running, locale), max: formatCount(facts.slots, locale) },
			{ locale }
		);
	}
	return '';
}

export function sortName(sort: SortKey, locale: Locale = getLocale()): string {
	const options = { locale };
	switch (sort) {
		case 'added':
			return m.col_added({}, options);
		case 'name':
			return m.col_name({}, options);
		case 'size':
			return m.col_size({}, options);
		case 'progress':
			return m.col_progress({}, options);
	}
}

/** "Newest first", "A → Z"…: what the direction means for that key. */
export function directionName(
	sort: SortKey,
	direction: Direction,
	locale: Locale = getLocale()
): string {
	const options = { locale };
	const up = direction === 'asc';
	switch (sort) {
		case 'added':
			return up ? m.sort_oldest({}, options) : m.sort_newest({}, options);
		case 'name':
			return up ? m.sort_a_z({}, options) : m.sort_z_a({}, options);
		case 'size':
			return up ? m.sort_smallest({}, options) : m.sort_largest({}, options);
		case 'progress':
			return up ? m.sort_least_done({}, options) : m.sort_most_done({}, options);
	}
}

const KEY = 'ottie.arrangement';

/** The arrangement last used on this device; the default when there's none
 * or it can't be read. */
export function loadArrangement(): Arrangement {
	try {
		return parseArrangement(localStorage.getItem(KEY));
	} catch {
		return DEFAULT_ARRANGEMENT; // Storage blocked.
	}
}

export function saveArrangement(arrangement: Arrangement): void {
	try {
		localStorage.setItem(KEY, JSON.stringify(arrangement));
	} catch {
		// Storage blocked: the default comes back next time.
	}
}

/** Reads what `saveArrangement` wrote, keeping the default for anything
 * missing or unknown (an older or newer page may have written it). */
export function parseArrangement(stored: string | null): Arrangement {
	let value: unknown = null;
	try {
		value = JSON.parse(stored ?? 'null');
	} catch {
		return DEFAULT_ARRANGEMENT;
	}
	if (typeof value !== 'object' || value === null) {
		return DEFAULT_ARRANGEMENT;
	}
	const { sort, direction, grouped } = value as Record<string, unknown>;
	return {
		sort: SORT_KEYS.find((key) => key === sort) ?? DEFAULT_ARRANGEMENT.sort,
		direction:
			direction === 'asc' || direction === 'desc' ? direction : DEFAULT_ARRANGEMENT.direction,
		grouped: typeof grouped === 'boolean' ? grouped : DEFAULT_ARRANGEMENT.grouped
	};
}
