// Which of the Downloads list's rows are on the page (docs/decisions/0061).
// A long list draws only those in view and some around them; the room the
// others would take is kept above and below, so it scrolls as if they were
// all there.

import type { Job } from '@ottie/api-types/Job';
import type { Section } from './arrange';

/** Lists of up to this many downloads draw every row (and keep every
 * animation, docs/decisions/0048); longer ones only those in view. */
export const DRAW_ALL_UP_TO = 200;

/** A row of the list: a group's heading, or a download. */
export type Line =
	| { kind: 'heading'; key: string; section: Section }
	| { kind: 'job'; key: string; job: Job; section: Section };

/** The list's rows, top to bottom: each group's heading (when grouped), then
 * its downloads. */
export function lines(sections: Section[]): Line[] {
	return sections.flatMap((section): Line[] => {
		const jobs = section.jobs.map((job): Line => ({ kind: 'job', key: job.id, job, section }));
		return section.group === null
			? jobs
			: [{ kind: 'heading', key: `group ${section.group}`, section }, ...jobs];
	});
}

/** Heights measured on the page, in pixels: a heading, a download's row, and
 * the space between two rows. */
export interface Sizes {
	heading: number;
	row: number;
	gap: number;
}

/** What a long list draws: lines `from` to `to` (not included), and the room
 * those before and after take, in pixels. */
export interface Drawn {
	from: number;
	to: number;
	above: number;
	below: number;
}

function height(line: Line, sizes: Sizes): number {
	return line.kind === 'heading' ? sizes.heading : sizes.row;
}

/** The lines in view when `height` pixels are seen from `top` (from the first
 * line's top), with `margin` pixels more above and below. */
export function drawn(all: Line[], sizes: Sizes, top: number, seen: number, margin: number): Drawn {
	const start = top - margin;
	const end = top + seen + margin;
	let y = 0;
	let from = all.length;
	let to = all.length;
	let above = 0;
	for (let i = 0; i < all.length; i++) {
		const tall = height(all[i], sizes);
		if (from === all.length && y + tall > start) {
			from = i;
			above = y;
		}
		if (y >= end) {
			to = i;
			break;
		}
		y += tall + sizes.gap;
	}
	if (from === all.length) {
		above = y; // Scrolled past the end (the list just got shorter).
	}
	// Each row is followed by a gap; a stand-in row is too.
	let rest = 0;
	for (let i = to; i < all.length; i++) {
		rest += height(all[i], sizes) + sizes.gap;
	}
	return {
		from,
		to,
		above: Math.max(0, above - sizes.gap),
		below: Math.max(0, rest - sizes.gap)
	};
}

/** Where line `index` is: its top, from the first line's top, and its height. */
export function place(all: Line[], sizes: Sizes, index: number): { top: number; height: number } {
	let top = 0;
	for (let i = 0; i < index && i < all.length; i++) {
		top += height(all[i], sizes) + sizes.gap;
	}
	return { top, height: all[index] ? height(all[index], sizes) : 0 };
}

/** How far to scroll (`scrollTop`) so a line at `spot` is in view, as little
 * as needed; `top` is what's scrolled now and `seen` the height in view,
 * `covered` of it hidden at the top (the sticky header). Unchanged if it's
 * in view already. */
export function scrollFor(
	spot: { top: number; height: number },
	top: number,
	seen: number,
	covered: number
): number {
	if (spot.top < top + covered) {
		return spot.top - covered;
	}
	if (spot.top + spot.height > top + seen) {
		return spot.top + spot.height - seen;
	}
	return top;
}
