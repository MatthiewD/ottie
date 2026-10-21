<script lang="ts">
	import type { Category } from '@ottie/api-types/Category';
	import type { Job } from '@ottie/api-types/Job';
	import type { View } from '@ottie/api-types/View';
	import Bookmark from '@lucide/svelte/icons/bookmark';
	import Ellipsis from '@lucide/svelte/icons/ellipsis';
	import { resolve } from '$app/paths';
	import type { Arrangement } from '#lib/arrange.js';
	import { count, filtersQuery, quickFilters, sameFilters, type Filters } from '#lib/filters.js';
	import { formatCount } from '#lib/format.js';
	import { m } from '#lib/paraglide/messages.js';
	import { drop, reshapes, shuffle, unfold } from '#lib/moving.svelte.js';
	import { showsView, viewFilters } from '#lib/views.js';

	interface Props {
		filters: Filters;
		arrangement: Arrangement;
		/** Every download, for the counts. */
		jobs: Job[];
		/** Categories that have downloads: each gets a pill behind "…". */
		categories: Category[];
		/** Saved views: each gets a pill after the fixed ones. */
		views: View[];
		now: Date;
		/** Picking a view also lays the list out its way. */
		onArrange: (arrangement: Arrangement) => void;
		onRename: (view: View) => void;
		onDelete: (view: View) => void;
		/** A read-only device: the views without their menu. */
		readOnly?: boolean;
	}

	let {
		filters,
		arrangement,
		jobs,
		categories,
		views,
		now,
		onArrange,
		onRename,
		onDelete,
		readOnly = false
	}: Props = $props();
	const downloads = resolve('/downloads');
	/** Any time: what `count` gets for a pill not by date, so only those by
	 * date are counted again as the clock moves. */
	const EPOCH = new Date(0);

	let pills = $derived(quickFilters(categories));
	let opened = $state(false);
	// A pill behind "…" that's in use keeps them shown (and "Less" hidden).
	let held = $derived(pills.more.some((pill) => sameFilters(pill.filters, filters)));
	let more = $derived(opened || held);
	let shown = $derived(more ? [...pills.main, ...pills.more] : pills.main);
	let extra = $derived(shown.slice(pills.main.length));
	/** The view whose menu is open. */
	let menu: number | null = $state(null);

	let track: HTMLElement | undefined = $state();
	/** The pill that carries the raised highlight (a view before a fixed pill,
	 * if both fit the filters), so it can slide to the next one chosen. */
	let lit = $derived(
		(() => {
			const view = views.find((view) => showsView(view, filters, arrangement));
			if (view) {
				return `view ${view.id}`;
			}
			return shown.find((pill) => sameFilters(pill.filters, filters))?.key ?? null;
		})()
	);
	// The highlight slides from pill to pill, and the track grows to show the
	// pills behind "…" and shrinks back (docs/decisions/0048).
	shuffle(
		() => track,
		() => lit
	);
	reshapes(
		() => track,
		() => more
	);

	/** Closes the menu once focus leaves it. */
	function focusOut(event: FocusEvent & { currentTarget: HTMLElement }) {
		const next = event.relatedTarget;
		if (!(next instanceof Node && event.currentTarget.contains(next))) {
			menu = null;
		}
	}

	function choose(action: (view: View) => void, view: View) {
		menu = null;
		action(view);
	}
</script>

<!-- Each pill is a link, so it can be bookmarked; switching replaces the
     history entry. -->
<nav class="pills" aria-label={m.filters_label()} bind:this={track}>
	{#each pills.main as pill (pill.key)}
		{@render link(pill.filters, pill.name, sameFilters(pill.filters, filters), pill.key)}
	{/each}
	{#each views as view (view.id)}
		{@const current = showsView(view, filters, arrangement)}
		<div class="view" class:current onfocusout={focusOut}>
			{@render link(viewFilters(view), view.name, current, `view ${view.id}`, view)}
			{#if current && !readOnly}
				<button
					class="options"
					in:unfold={{ axis: 'x' }}
					out:unfold={{ axis: 'x' }}
					aria-haspopup="menu"
					aria-expanded={menu === view.id}
					aria-label={m.view_options({ name: view.name })}
					title={m.view_options({ name: view.name })}
					onclick={() => (menu = menu === view.id ? null : view.id)}
				>
					<Ellipsis size={15} aria-hidden="true" />
				</button>
				{#if menu === view.id}
					<ul role="menu" in:drop out:drop>
						<li role="none">
							<button role="menuitem" onclick={() => choose(onRename, view)}>
								{m.view_rename()}
							</button>
						</li>
						<li role="none">
							<button role="menuitem" class="danger" onclick={() => choose(onDelete, view)}>
								{m.view_delete()}
							</button>
						</li>
					</ul>
				{/if}
			{/if}
		</div>
	{/each}
	{#each extra as pill (pill.key)}
		{@render link(pill.filters, pill.name, sameFilters(pill.filters, filters), pill.key)}
	{/each}
	{#if !held}
		<button
			class="more"
			aria-expanded={more}
			aria-label={more ? m.filters_fewer() : m.filters_more()}
			title={more ? m.filters_fewer() : m.filters_more()}
			onclick={() => (opened = !opened)}
		>
			{more ? m.filters_less() : '…'}
		</button>
	{/if}
</nav>

{#snippet link(target: Filters, name: string, current: boolean, id: string, view?: View)}
	<a
		href={downloads + filtersQuery(target)}
		class:current
		class:lit={id === lit}
		aria-current={current ? 'true' : undefined}
		onclick={() => view && onArrange(view.arrangement)}
		data-sveltekit-replacestate
		data-sveltekit-noscroll
		data-sveltekit-keepfocus
	>
		{#if id === lit}
			<span class="highlight" data-flip="highlight" data-flip-size></span>
		{/if}
		{#if view}<Bookmark size={13} aria-hidden="true" />{/if}
		<span class="label">
			<span dir="auto">{name}</span>
			<span class="count"
				>{formatCount(count(jobs, target, target.added === 'any' ? EPOCH : now))}</span
			>
		</span>
	</a>
{/snippet}

<style>
	/* The pills sit in a sunken track, the current one raised; saved views
	   and "…" stay apart, after it. */
	.pills {
		/* Its own layer: the chosen one's highlight slides under every
		   item's text (each item isolated, it would cover the earlier ones). */
		isolation: isolate;
		display: flex;
		flex-wrap: wrap;
		gap: 0.15rem;
		width: fit-content;
		max-width: 100%;
		padding: 0.25rem;
		border-radius: 1.2rem;
		background: var(--sunken);
		box-shadow: var(--sunken-shadow);
	}

	a,
	.more {
		position: relative;
		display: flex;
		flex: none;
		align-items: center;
		gap: 0.45rem;
		height: 2rem;
		padding: 0 0.8rem;
		border: 1px solid transparent;
		border-radius: 999px;
		color: var(--muted);
		font-size: 0.9em;
		font-weight: 500;
		text-decoration: none;
	}

	a:not(.lit):hover {
		background: var(--hover);
		color: var(--text);
	}

	a.current {
		color: var(--text);
		font-weight: 600;
	}

	/* The highlight is an element of its own (`.highlight`, app.css), unless
	   two pills are current at once and it sits on the other. */
	a.current:not(.lit) {
		background: var(--raised);
		box-shadow: var(--raised-shadow);
	}

	/* The name and its count on one baseline (each centred on its own, the
	   smaller count sits too high). */
	.label {
		display: flex;
		align-items: baseline;
		gap: 0.45rem;
	}

	.count {
		font-family: var(--mono);
		font-size: 0.8em;
		font-weight: 400;
		opacity: 0.75;
	}

	/* A saved view: its pill, and while it's shown, the button of its menu
	   joined to the pill's right end. */
	.view {
		position: relative;
		display: flex;
	}

	.view a :global(svg) {
		margin-inline-start: -0.2rem;
		opacity: 0.8;
	}

	.view.current a {
		padding-inline-end: 0.55rem;
		border-top-right-radius: 0;
		border-bottom-right-radius: 0;
	}

	.options {
		display: flex;
		align-items: center;
		height: 2rem;
		padding: 0 0.5rem 0 0.4rem;
		border: none;
		border-inline-start: 1px solid var(--border);
		border-radius: 0 999px 999px 0;
		background: var(--raised);
		box-shadow: var(--raised-shadow);
		color: var(--text);
	}

	.options:hover {
		filter: brightness(1.1);
	}

	ul {
		position: absolute;
		top: calc(100% + 0.3rem);
		inset-inline-end: 0;
		z-index: 10;
		min-width: 10rem;
		margin: 0;
		padding: 0.3rem;
		list-style: none;
		border: 1px solid var(--edge);
		border-radius: var(--radius-m);
		background: var(--surface);
		box-shadow: var(--shadow);
	}

	ul button {
		width: 100%;
		height: 2.1rem;
		padding: 0 0.6rem;
		border-color: transparent;
		border-radius: var(--radius-s);
		background: none;
		text-align: start;
	}

	ul button:hover {
		border-color: transparent;
		background: var(--hover);
	}

	ul .danger {
		color: var(--danger);
	}

	.more {
		justify-content: center;
		min-width: 2.5rem;
		border-color: var(--border);
		border-style: dashed;
		background: none;
		font-weight: 600;
	}

	.more:hover {
		background: var(--hover);
	}
</style>
