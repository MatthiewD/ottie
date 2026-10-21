// The job list as the daemon describes it, kept up to date by events. Plain
// functions over a plain object, so they're easy to test; `session.svelte.ts`
// calls them on reactive state.

import type { Event as ApiEvent } from '@ottie/api-types/Event';
import type { Job } from '@ottie/api-types/Job';

export type Jobs = Record<string, Job>;

/** A download's recent speed. */
export interface Rate {
	/** When `downloaded` was seen, in milliseconds (`Date.now()`). */
	at: number;
	downloaded: number;
	bytesPerSec: number;
}

/** Applies one event. Returns true when events were missed and the whole
 * list must be fetched again. */
export function applyEvent(jobs: Jobs, event: ApiEvent): boolean {
	switch (event.type) {
		case 'changed':
			jobs[event.job.id] = event.job;
			return false;
		case 'progress': {
			const job = jobs[event.id];
			if (job) {
				job.downloaded = event.downloaded;
				job.total = event.total;
				job.ranges = event.ranges;
			}
			return false;
		}
		case 'removed':
			delete jobs[event.id];
			return false;
		// Not about jobs (moved jobs get their own `changed` events).
		case 'categoryChanged':
		case 'categoryRemoved':
		case 'settingsChanged':
		case 'viewChanged':
		case 'viewRemoved':
		case 'slotChanged':
		case 'slotRemoved':
			return false;
		case 'resync':
			return true;
	}
}

/** IDs sort by creation time; `numeric` makes `…-10` come after `…-9`.
 * One collator for every comparison: `localeCompare` with options makes a
 * new one each time, and sorting 10,000 downloads took a third of a second. */
const IDS = new Intl.Collator(undefined, { numeric: true });

/** Oldest first. */
export function byCreation(a: Job, b: Job): number {
	return IDS.compare(a.id, b.id);
}

/** Newest first, so what you just added is at the top. */
export function newestFirst(jobs: Jobs): Job[] {
	return Object.values(jobs).sort((a, b) => byCreation(b, a));
}

/** How much a new measurement moves the shown speed (the rest is history), so
 * it doesn't jump around with every chunk. */
const SMOOTHING = 0.3;
/** Measurements closer than this are too noisy to use. */
const MIN_INTERVAL_MS = 200;

export function nextRate(previous: Rate | undefined, downloaded: number, now: number): Rate {
	// First sight, or the download restarted from zero.
	if (!previous || downloaded < previous.downloaded) {
		return { at: now, downloaded, bytesPerSec: 0 };
	}
	const elapsed = now - previous.at;
	if (elapsed < MIN_INTERVAL_MS) {
		return previous;
	}
	const measured = ((downloaded - previous.downloaded) * 1000) / elapsed;
	const bytesPerSec =
		previous.bytesPerSec === 0
			? measured
			: previous.bytesPerSec + SMOOTHING * (measured - previous.bytesPerSec);
	return { at: now, downloaded, bytesPerSec };
}

/** No progress for this long means the shown speed is stale. */
export const STALE_AFTER_MS = 3000;

/** The speed to show at `now`: 0 when nothing has arrived for a while. */
export function currentSpeed(rate: Rate | undefined, now: number): number {
	if (!rate || now - rate.at > STALE_AFTER_MS) {
		return 0;
	}
	return rate.bytesPerSec;
}

/** `2026.10.12-14.03.07-1`: the creation time in UTC, then a counter. */
const ID = /^(\d{4})\.(\d{2})\.(\d{2})-(\d{2})\.(\d{2})\.(\d{2})-\d+$/;

/** When the job was added, read from its ID; null if the ID isn't one. */
export function addedAt(id: string): Date | null {
	const match = ID.exec(id);
	if (!match) {
		return null;
	}
	const [year, month, day, hours, minutes, seconds] = match.slice(1).map(Number);
	return new Date(Date.UTC(year, month - 1, day, hours, minutes, seconds));
}
