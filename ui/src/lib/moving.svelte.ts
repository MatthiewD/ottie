// The page's animations (docs/decisions/0048): Svelte transitions for what
// comes and goes (dialogs, menus, rows), and lists whose rows slide into
// their new places. The plain logic is in motion.ts.

import { untrack } from 'svelte';
import { cubicIn, cubicOut } from 'svelte/easing';
import { prefersReducedMotion } from 'svelte/motion';
import { slide, type TransitionConfig } from 'svelte/transition';
import { appearance } from './appearance.svelte';
import { CROWD, MANY, QUICK, SETTLE, STEADY, moves, plan, type Spot } from './motion';

/** Whether the page moves now (live: the system's setting can change). */
export function moving(): boolean {
	return moves(appearance.motion, prefersReducedMotion.current);
}

/** Svelte calls a transition with what it does; `in:` and `out:` say so. */
interface Options {
	direction: 'in' | 'out' | 'both';
}

const STILL: TransitionConfig = { duration: 0 };

/** What an element's own `transform` is, so a transition adds to it rather
 * than replacing it (a bubble centred with `translate`). */
function baseTransform(node: Element): string {
	const transform = getComputedStyle(node).transform;
	return transform === 'none' ? '' : `${transform} `;
}

/** A dialog: grows from a little smaller as it fades in, and its backdrop
 * fades with it (`.leaving::backdrop` in app.css). */
export function pop(node: Element, _: unknown, { direction }: Options): TransitionConfig {
	if (!moving()) {
		return STILL;
	}
	const leaving = direction === 'out';
	if (leaving) {
		node.classList.add('leaving');
	}
	const base = baseTransform(node);
	return {
		duration: leaving ? QUICK : STEADY,
		easing: leaving ? cubicIn : cubicOut,
		css: (t) => `opacity: ${t}; transform: ${base}scale(${0.96 + 0.04 * t})`
	};
}

/** A menu or popup: fades in as it drops a little from its button (or rises,
 * when it opens upwards). */
export function drop(
	node: Element,
	{ up = false }: { up?: boolean } = {},
	{ direction }: Options
): TransitionConfig {
	if (!moving()) {
		return STILL;
	}
	const leaving = direction === 'out';
	const base = baseTransform(node);
	const from = up ? 4 : -4;
	return {
		duration: leaving ? QUICK : STEADY,
		easing: leaving ? cubicIn : cubicOut,
		css: (t, u) => `opacity: ${t}; transform: ${base}translateY(${from * u}px)`
	};
}

/** Something that appears where there was nothing (an error under a form,
 * fields a switch reveals, a menu's choices): unfolds to its size, the rest
 * making room as it goes, and folds up again. `axis: 'x'` for what joins a
 * row (a toolbar's button). */
export function unfold(
	node: Element,
	{ axis = 'y' }: { axis?: 'x' | 'y' } = {},
	{ direction }: Options
): TransitionConfig {
	if (!moving()) {
		return STILL;
	}
	const leaving = direction === 'out';
	return slide(node, {
		axis,
		duration: leaving ? QUICK : STEADY,
		easing: leaving ? cubicIn : cubicOut
	});
}

/** Something that takes the place of another (the filters' pills and panel):
 * fades in. */
export function appear(_node: Element): TransitionConfig {
	return moving() ? { duration: STEADY, easing: cubicOut, css: (t) => `opacity: ${t}` } : STILL;
}

/** A row going away: it fades where it was while the others slide into its
 * place (`shuffle`), mostly gone before they reach it. Only opacity:
 * `shuffle` holds it in place with `transform`. With `fades` false, it goes
 * at once. */
export function leave(_node: Element, fades = true): TransitionConfig {
	return moving() && fades
		? { duration: QUICK, easing: cubicOut, css: (t) => `opacity: ${t}` }
		: STILL;
}

/** Runs `before` ahead of the page's update after `watch()` changes, and
 * `after` with what it returned once the update is on the page. Not when
 * it first shows, nor while the page doesn't move. */
function aroundChanges<T>(watch: () => unknown, before: () => T | null, after: (t: T) => void) {
	// As text, so an equal list doesn't count as a change.
	const changed = $derived(JSON.stringify(watch()));
	let measured: T | null = null;
	let first = true;
	$effect.pre(() => {
		void changed;
		if (first) {
			first = false;
			return;
		}
		measured = untrack(() => (moving() ? before() : null));
	});
	$effect(() => {
		void changed;
		const taken = measured;
		measured = null;
		if (taken !== null) {
			untrack(() => after(taken));
		}
	});
}

/** The animations a list starts itself, so they can be stopped. */
const SHUFFLE = 'ottie-shuffle';

/** A list whose rows slide to their new places when `watch()` changes
 * (reordered, sorted, one added or removed above them), fade in when they
 * come and fade out where they were when they go (with `out:leave`). Each
 * row carries `data-flip` with its key; a row with `data-flip-size` also
 * grows or shrinks from its old size (a tab's highlight). Rows that stay
 * keep their key: one that moves from one group to another with the same
 * key slides there. */
export function shuffle(root: () => HTMLElement | undefined, watch: () => unknown): void {
	interface Row {
		element: HTMLElement;
		spot: Spot;
	}

	// Marked, so a list inside another (the dashboard's span picker) keeps
	// its rows to itself.
	$effect(() => {
		const list = root();
		if (list) {
			list.dataset.shuffle = '';
		}
	});

	/** The rows shown, where they are (stopping the slides still under way
	 * first, if asked), or null if there are too many to move. */
	function rows(list: HTMLElement, settled: boolean): Map<string, Row> | null {
		const elements = [...list.querySelectorAll<HTMLElement>('[data-flip]')].filter(
			// Already going (a fade under way), or another list's.
			(element) => !element.inert && element.parentElement?.closest('[data-shuffle]') === list
		);
		if (elements.length > CROWD) {
			return null;
		}
		if (settled) {
			for (const element of elements) {
				for (const animation of element.getAnimations()) {
					if (animation.id === SHUFFLE) {
						animation.cancel();
					}
				}
			}
		}
		const found = new Map<string, Row>();
		for (const element of elements) {
			found.set(element.dataset.flip ?? '', { element, spot: element.getBoundingClientRect() });
		}
		return found;
	}

	aroundChanges(
		watch,
		() => {
			const list = root();
			return list ? rows(list, false) : null;
		},
		(before) => {
			const list = root();
			const after = list ? rows(list, true) : null;
			if (after === null) {
				return;
			}
			const steps = plan(
				new Map([...before].map(([key, row]) => [key, row.spot])),
				new Map([...after].map(([key, row]) => [key, row.spot])),
				innerHeight
			);
			// Those going stay where they were, out of the way of the others.
			for (const key of [...steps.fading, ...steps.hidden]) {
				const row = before.get(key);
				if (row?.element.isConnected) {
					holdInPlace(row.element, row.spot, steps.hidden.includes(key));
				}
			}
			// A row that moved to another group was made again there; its old
			// self fades nowhere.
			for (const [key, row] of before) {
				if (after.has(key) && row.element !== after.get(key)?.element && row.element.isConnected) {
					holdInPlace(row.element, row.spot, true);
				}
			}
			const options = { duration: STEADY, easing: SETTLE, id: SHUFFLE };
			for (const { key, dx, dy, sx, sy } of steps.moved) {
				const row = after.get(key);
				if (row === undefined) {
					continue;
				}
				const { element, spot } = row;
				const sized = element.dataset.flipSize !== undefined;
				if (dx !== 0 || dy !== 0 || (sized && (sx !== 1 || sy !== 1))) {
					const scale = sized ? ` scale(${sx}, ${sy})` : '';
					element.animate(
						[
							{ transform: `translate(${dx}px, ${dy}px)${scale}`, transformOrigin: '0 0' },
							{ transform: 'none', transformOrigin: '0 0' }
						],
						options
					);
				}
				// A row that grew (an editor in its place) opens up as the rows
				// under it slide down.
				if (!sized && sy < 1) {
					grow(element, spot.height * sy, spot.height, options);
				}
			}
			// A list filled at once (the downloads arriving as the page
			// starts): too many to fade one by one, so it fades as a whole.
			if (list && before.size === 0 && after.size > MANY) {
				list.animate([{ opacity: 0 }, { opacity: 1 }], options);
			}
			for (const key of steps.entered) {
				const row = after.get(key);
				row?.element.animate([{ opacity: 0 }, { opacity: 1 }], options);
				// Opening up the room it takes, as the rows under it slide down.
				if (row) {
					grow(row.element, 0, row.spot.height, options);
				}
			}
		}
	);
}

/** Uncovers an element that grew, from `from` pixels high to all of it
 * (`to`), in step with the rows sliding down under it. A clip, not its
 * height: that would push those rows a second time. */
function grow(element: HTMLElement, from: number, to: number, options: KeyframeAnimationOptions) {
	if (to - from < 1) {
		return;
	}
	element.animate([{ clipPath: `inset(0 0 ${to - from}px 0)` }, { clipPath: 'inset(0)' }], options);
}

/** Takes a row that's going out of the list's flow, drawn where it was
 * (`spot`), so the others can take its place while it fades; or hidden. */
function holdInPlace(element: HTMLElement, spot: Spot, hidden: boolean) {
	const style = element.style;
	// A table row on its own is laid out as a little table with its cells'
	// widths, or they'd shrink to their text.
	if (element instanceof HTMLTableRowElement) {
		const widths = [...element.cells].map((cell) => cell.getBoundingClientRect().width);
		style.display = 'table';
		style.tableLayout = 'fixed';
		[...element.cells].forEach((cell, i) => {
			cell.style.boxSizing = 'border-box';
			cell.style.width = `${widths[i]}px`;
		});
	}
	style.position = 'absolute';
	style.width = `${spot.width}px`;
	style.height = `${spot.height}px`;
	style.margin = '0';
	style.pointerEvents = 'none';
	if (hidden) {
		style.visibility = 'hidden';
	}
	const now = element.getBoundingClientRect();
	style.transform = `translate(${spot.left - now.left}px, ${spot.top - now.top}px)`;
}

/** Elements whose height changes with `watch()` (a panel folding, a
 * section whose settings arrive) grow or shrink to it rather than jumping,
 * and what they hold that's new fades in. */
export function resizes(
	elements: () => HTMLElement | HTMLElement[] | undefined,
	watch: () => unknown
): void {
	aroundChanges(
		watch,
		() => {
			const found = elements();
			const nodes = found === undefined ? [] : Array.isArray(found) ? found : [found];
			return nodes.map((node) => ({
				node,
				from: node.getBoundingClientRect().height,
				held: new Set(node.children)
			}));
		},
		(measured) => {
			for (const { node, from, held } of measured) {
				if (node.isConnected) {
					settle(node, from, (child) => !held.has(child));
				}
			}
		}
	);
}

/** One element replaced by another in the same place (the details panel and
 * its empty state, `find()` giving whichever is there): the new one grows or
 * shrinks from the old one's height and fades in. The same one changing
 * height (another download's details) only grows or shrinks. */
export function swaps(find: () => HTMLElement | null | undefined, watch: () => unknown): void {
	aroundChanges(
		watch,
		() => {
			const node = find();
			return node ? { node, from: node.getBoundingClientRect().height } : null;
		},
		({ node: old, from }) => {
			const node = find();
			if (node) {
				const fresh = node !== old;
				settle(node, from, () => fresh);
			}
		}
	);
}

/** A box whose content wraps (the filter pills' track) and whose width and
 * height change with `watch()`. Growing, it's laid out at its new size at
 * once and uncovered from its old one, so nothing in it wraps differently
 * on the way, and what's new in it fades in; shrinking (what it held is
 * gone, the rest fits), it narrows to its new size. */
export function reshapes(element: () => HTMLElement | undefined, watch: () => unknown): void {
	aroundChanges(
		watch,
		() => {
			const node = element();
			return node
				? { node, from: node.getBoundingClientRect(), held: new Set(node.children) }
				: null;
		},
		({ node, from, held }) => {
			if (!node.isConnected) {
				return;
			}
			for (const animation of node.getAnimations()) {
				if (animation.id === RESIZE) {
					animation.cancel();
				}
			}
			const options = { duration: STEADY, easing: SETTLE, id: RESIZE };
			const to = node.getBoundingClientRect();
			if (to.width >= from.width && to.height >= from.height) {
				for (const child of node.children) {
					if (!held.has(child)) {
						child.animate([{ opacity: 0 }, { opacity: 1 }], options);
					}
				}
				const style = getComputedStyle(node);
				const radius = style.borderStartStartRadius;
				const dx = to.width - from.width;
				const dy = to.height - from.height;
				// From the end of the line: the right, or the left right to left.
				const [right, left] = style.direction === 'rtl' ? [0, dx] : [dx, 0];
				node.animate(
					[
						{ clipPath: `inset(0 ${right}px ${dy}px ${left}px round ${radius})` },
						{ clipPath: `inset(0 0 0 0 round ${radius})` }
					],
					options
				);
			} else {
				node.animate(
					[
						{ width: `${from.width}px`, height: `${from.height}px`, overflow: 'clip' },
						{ width: `${to.width}px`, height: `${to.height}px`, overflow: 'clip' }
					],
					options
				);
			}
		}
	);
}

const RESIZE = 'ottie-resize';

/** Takes `node` from `from` pixels high to its height now, and fades in
 * those of its children `fades` picks. */
function settle(node: HTMLElement, from: number, fades: (child: Element) => boolean) {
	for (const animation of node.getAnimations()) {
		if (animation.id === RESIZE) {
			animation.cancel();
		}
	}
	const options = { duration: STEADY, easing: SETTLE, id: RESIZE };
	for (const child of node.children) {
		if (fades(child)) {
			child.animate([{ opacity: 0 }, { opacity: 1 }], options);
		}
	}
	const to = node.getBoundingClientRect().height;
	if (Math.abs(to - from) >= 1) {
		node.animate(
			// `clip`, not `hidden`: a field taking the focus can't scroll it.
			[
				{ height: `${from}px`, overflow: 'clip' },
				{ height: `${to}px`, overflow: 'clip' }
			],
			options
		);
	}
}

/** A page that takes the place of another (a tab): fades in, from half
 * seen rather than from nothing, which showed as a blank frame between the
 * two pages. */
export function fadeIn(element: Element): void {
	if (moving()) {
		element.animate([{ opacity: 0.5 }, { opacity: 1 }], { duration: STEADY, easing: SETTLE });
	}
}
