// The queue (docs/decisions/0032): waiting downloads start by priority, then
// by place. Moving downloads only reorders the waiting ones of their own
// priority, as the daemon does.

import type { Job } from '@ottie/api-types/Job';
import type { Priority } from '@ottie/api-types/Priority';
import type { Reorder } from '@ottie/api-types/Reorder';

export type { Reorder };

/** The highest priority first. */
const RANK: Record<Priority, number> = { urgent: 0, high: 1, normal: 2, low: 3 };

/** Queue order: the highest priority first, then the lowest place. */
export function byQueue(a: Job, b: Job): number {
	return RANK[a.priority] - RANK[b.priority] || a.place - b.place;
}

/** Queued, scheduled or paused: the downloads a move reorders. */
export function isWaiting(job: Job): boolean {
	return job.state === 'queued' || job.state === 'scheduled' || job.state === 'paused';
}

/** Every waiting download (queued, scheduled or paused), in queue order:
 * the dashboard's Waiting card. */
export function waitingInOrder(jobs: Job[]): Job[] {
	return jobs.filter(isWaiting).sort(byQueue);
}

/** Whether moving the `selected` downloads `to` would change anything: one
 * of them has a waiting download of its priority, not selected, just before
 * it (up, top) or after it (down, bottom). */
export function canReorder(jobs: Job[], selected: Job[], to: Reorder): boolean {
	if (selected.length === 0) {
		return false;
	}
	const chosen = new Set(selected.map((job) => job.id));
	const waiting = jobs.filter(isWaiting).sort(byQueue);
	const earlier = to === 'up' || to === 'top';
	return waiting.some((job, i) => {
		const next = waiting[i + 1];
		if (next === undefined || next.priority !== job.priority) {
			return false;
		}
		const [mover, other] = earlier ? [next, job] : [job, next];
		return chosen.has(mover.id) && !chosen.has(other.id);
	});
}

/** What dragging `grabbed` carries: the selected waiting downloads of its
 * priority if it's selected, else only itself. In queue order. */
export function carried(grabbed: Job, selected: Job[]): Job[] {
	if (!selected.some((job) => job.id === grabbed.id)) {
		return [grabbed];
	}
	return selected
		.filter((job) => isWaiting(job) && job.priority === grabbed.priority)
		.sort(byQueue);
}

/** Whether the `moving` downloads can be dropped next to `target`: a
 * waiting download of their priority that isn't one of them. */
export function canDrop(moving: Job[], target: Job): boolean {
	return (
		isWaiting(target) &&
		moving.length > 0 &&
		moving.every((job) => job.priority === target.priority && job.id !== target.id)
	);
}
