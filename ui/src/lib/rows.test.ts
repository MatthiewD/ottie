import { describe, expect, it } from 'vitest';
import type { Section } from './arrange';
import { drawn, lines, place, scrollFor, type Sizes } from './rows';
import { job } from './testing';

const jobs = (count: number, from = 0) =>
	Array.from({ length: count }, (_, i) => job({ id: `2026.10.21-10.00.00-${from + i}` }));

const SIZES: Sizes = { heading: 30, row: 40, gap: 2 };

describe('lines', () => {
	it('puts each group heading before its downloads', () => {
		const sections: Section[] = [
			{ group: 'running', jobs: jobs(2) },
			{ group: 'finished', jobs: jobs(1, 2) }
		];
		expect(lines(sections).map((line) => line.key)).toEqual([
			'group running',
			'2026.10.21-10.00.00-0',
			'2026.10.21-10.00.00-1',
			'group finished',
			'2026.10.21-10.00.00-2'
		]);
	});

	it('has no headings when not grouped', () => {
		const all = lines([{ group: null, jobs: jobs(3) }]);
		expect(all.every((line) => line.kind === 'job')).toBe(true);
	});
});

describe('drawn', () => {
	// A heading (30 + 2), then rows of 40 + 2.
	const all = lines([{ group: 'finished', jobs: jobs(1000) }]);
	const total = 32 + 1000 * 42 - 2;

	it('draws the top of the list when it starts', () => {
		const view = drawn(all, SIZES, 0, 420, 0);
		expect(view.from).toBe(0);
		expect(view.above).toBe(0);
		// The heading, then rows until 420 px: (420 − 32) / 42 → 10 rows.
		expect(view.to).toBe(11);
	});

	it('keeps the room of the rows it leaves out', () => {
		const view = drawn(all, SIZES, 4200, 420, 84);
		// Row k starts at 32 + 42k: the first drawn ends after 4200 − 84.
		expect(view.from).toBe(1 + 97);
		expect(view.above).toBe(32 + 97 * 42 - 2);
		// The stand-ins, the rows and the gaps add up to the whole list.
		const rows = (view.to - view.from) * 42;
		expect(view.above + 2 + rows + view.below).toBe(total);
	});

	it('draws the end of the list when scrolled to the bottom', () => {
		const view = drawn(all, SIZES, total - 420, 420, 0);
		expect(view.to).toBe(all.length);
		expect(view.below).toBe(0);
	});

	it('draws nothing past the end, keeping the room above', () => {
		const view = drawn(all, SIZES, total + 1000, 420, 0);
		expect(view.from).toBe(view.to);
		expect(view.above).toBe(total);
	});

	it('draws nothing in an empty list', () => {
		expect(drawn([], SIZES, 0, 420, 600)).toEqual({ from: 0, to: 0, above: 0, below: 0 });
	});
});

describe('place and scrollFor', () => {
	const all = lines([{ group: 'finished', jobs: jobs(100) }]);

	it('finds where a row is', () => {
		expect(place(all, SIZES, 0)).toEqual({ top: 0, height: 30 });
		expect(place(all, SIZES, 3)).toEqual({ top: 32 + 2 * 42, height: 40 });
	});

	it('scrolls as little as needed, below the sticky header', () => {
		// In view already.
		expect(scrollFor({ top: 500, height: 40 }, 400, 400, 35)).toBe(400);
		// Above: just under the header.
		expect(scrollFor({ top: 300, height: 40 }, 400, 400, 35)).toBe(265);
		// Below: its bottom at the bottom.
		expect(scrollFor({ top: 900, height: 40 }, 400, 400, 35)).toBe(540);
	});
});
