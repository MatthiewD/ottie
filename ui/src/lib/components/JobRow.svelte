<script lang="ts">
	import type { Component } from 'svelte';
	import type { Job } from '@ottie/api-types/Job';
	import type { Priority } from '@ottie/api-types/Priority';
	import ArrowBigDown from '@lucide/svelte/icons/arrow-big-down';
	import ArrowBigUp from '@lucide/svelte/icons/arrow-big-up';
	import Flame from '@lucide/svelte/icons/flame';
	import GripVertical from '@lucide/svelte/icons/grip-vertical';
	import Pause from '@lucide/svelte/icons/pause';
	import Play from '@lucide/svelte/icons/play';
	import Trash from '@lucide/svelte/icons/trash';
	import type { Action } from '#lib/bulk.js';
	import { priorityName } from '#lib/details.js';
	import { displayName, formatAdded, formatBytes, formatPercent } from '#lib/format.js';
	import { addedAt } from '#lib/jobs.js';
	import { leave } from '#lib/moving.svelte.js';
	import { m } from '#lib/paraglide/messages.js';
	import type { Keys } from '#lib/selection.js';
	import { canPause, canResume, describe, fraction, motion } from '#lib/status.js';
	import FileBar from './FileBar.svelte';
	import FileIcon from './FileIcon.svelte';
	import { useApp } from '#lib/context.js';

	interface Props {
		job: Job;
		selected: boolean;
		/** The name of its category. */
		category: string;
		/** Bytes per second right now; 0 if unknown. */
		speed: number;
		now: number;
		onClick: (keys: Keys) => void;
		onRun: (action: Action) => void;
		/** A double-click opens its file; null if it can't. */
		onOpen: (() => void) | null;
		/** The table has a column of grips (grouped by status). */
		grip: boolean;
		/** It can be dragged in the queue (it's waiting). */
		movable: boolean;
		/** It's being dragged. */
		carried: boolean;
		/** Where a drag would drop, next to this row. */
		drop: 'before' | 'after' | null;
		/** It fades as it goes; not in a long list, where it also goes when
		 * scrolled out of sight (docs/decisions/0061). */
		fades: boolean;
		onGrab: (event: PointerEvent) => void;
		/** The arrow keys on its grip. */
		onStep: (to: 'up' | 'down') => void;
	}

	let {
		job,
		selected,
		category,
		speed,
		now,
		onClick,
		onRun,
		onOpen,
		grip,
		movable,
		carried,
		drop,
		fades,
		onGrab,
		onStep
	}: Props = $props();
	const { session } = useApp();

	/** Normal has none. */
	const PRIORITY_ICONS: Record<Exclude<Priority, 'normal'>, Component> = {
		urgent: Flame,
		high: ArrowBigUp,
		low: ArrowBigDown
	};

	let name = $derived(displayName(job));
	let done = $derived(fraction(job));
	let moving = $derived(motion(job, speed, now, undefined, session.slotNames));
	let added = $derived(addedAt(job.id));
	let size = $derived(
		job.total !== null
			? formatBytes(job.total)
			: job.state === 'completed'
				? formatBytes(job.downloaded)
				: '—'
	);

	function step(event: KeyboardEvent) {
		if (event.key === 'ArrowUp' || event.key === 'ArrowDown') {
			event.preventDefault();
			onStep(event.key === 'ArrowUp' ? 'up' : 'down');
		}
	}

	function click(event: MouseEvent) {
		onClick({ toggle: event.ctrlKey || event.metaKey, range: event.shiftKey });
	}

	/** On the row, not on its own buttons and checkbox. */
	function doubleClick(event: MouseEvent) {
		if (!(event.target instanceof Element && event.target.closest('button, input, a'))) {
			onOpen?.();
		}
	}

	/** The row's own buttons and checkbox don't change the selection like a row click. */
	function run(event: MouseEvent, action: Action) {
		event.stopPropagation();
		onRun(action);
	}
</script>

<!-- Clicking the row selects it, as in a file manager; from the keyboard the
     row's checkbox does the same. -->
<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
<tr
	class={job.state}
	class:selected
	class:carried
	class:drop-before={drop === 'before'}
	class:drop-after={drop === 'after'}
	data-id={job.id}
	data-flip={job.id}
	out:leave={fades}
	onclick={click}
	ondblclick={onOpen ? doubleClick : undefined}
>
	{#if grip}
		<td class="grip">
			{#if movable}
				<button
					class="handle"
					aria-label={m.queue_drag({ name })}
					title={m.queue_drag({ name })}
					onpointerdown={onGrab}
					onclick={(event) => event.stopPropagation()}
					onkeydown={step}
				>
					<GripVertical size={14} aria-hidden="true" />
				</button>
			{/if}
		</td>
	{/if}
	<td class="check">
		<input
			type="checkbox"
			checked={selected}
			aria-label={m.select_row({ name })}
			onclick={(event) => {
				event.stopPropagation();
				onClick({ toggle: true, range: event.shiftKey });
			}}
		/>
	</td>
	<td class="name" title={job.url}>
		<div class="cell">
			<span class="kind"><FileIcon {name} /></span>
			<span class="text">
				<span class="title" dir="auto">{name}</span>
				<span class="line">{describe(job, speed, now, undefined, session.slotNames)}</span>
			</span>
			{#if job.priority !== 'normal'}
				{@const Icon = PRIORITY_ICONS[job.priority]}
				{@const label = m.priority_icon({ priority: priorityName(job.priority) })}
				<span class="priority {job.priority}" role="img" aria-label={label} title={label}>
					<Icon size={14} aria-hidden="true" />
				</span>
			{/if}
		</div>
	</td>
	<td class="size">{size}</td>
	<td class="progress">
		<div class="cell">
			<!-- A small copy of the details panel's bar (docs/decisions/0044). -->
			<FileBar
				{job}
				size="row"
				role="progressbar"
				aria-label={m.progress_label({ name })}
				aria-valuemin={0}
				aria-valuemax={100}
				aria-valuenow={done === null ? undefined : Math.floor(done * 100)}
			/>
			<span class="percent">{done === null ? '' : formatPercent(done)}</span>
		</div>
	</td>
	{#if 'status' in moving}
		<td class="status" colspan="2" title={moving.status}>{moving.status}</td>
	{:else}
		<td class="speed">{moving.speed}</td>
		<td class="left">{moving.left}</td>
	{/if}
	<td class="category" title={category}>{category}</td>
	<td class="added">{added ? formatAdded(added) : ''}</td>
	<td class="actions">
		{#if !session.readOnly}
			{#if canPause(job)}
				<button
					onclick={(event) => run(event, 'pause')}
					aria-label={m.pause_label({ name })}
					title={m.pause()}
				>
					<Pause size={15} />
				</button>
			{:else if canResume(job)}
				<button
					onclick={(event) => run(event, 'resume')}
					aria-label={m.resume_label({ name })}
					title={m.resume()}
				>
					<Play size={15} />
				</button>
			{/if}
			<button
				onclick={(event) => run(event, 'remove')}
				aria-label={m.remove_label({ name })}
				title={m.remove()}
			>
				<Trash size={15} />
			</button>
		{/if}
	</td>
</tr>

<style>
	tr {
		height: 2.6rem;
		cursor: default;
		user-select: none;
		/* Reached with the arrow keys: shown below the sticky header. */
		scroll-margin-block-start: 2.2rem;
	}

	tr.selected {
		background: var(--selected);
	}

	tr.carried {
		opacity: 0.5;
	}

	/* Where a dragged download would land. */
	tr.drop-before td {
		box-shadow: inset 0 2px 0 var(--accent);
	}

	tr.drop-after td {
		box-shadow: inset 0 -2px 0 var(--accent);
	}

	.grip {
		padding: 0;
		text-align: center;
	}

	.handle {
		display: inline-grid;
		place-items: center;
		width: 1.4rem;
		height: 1.9rem;
		padding: 0;
		border-color: transparent;
		background: none;
		color: var(--faint);
		cursor: grab;
		/* A touch drag moves the download rather than scrolling the list. */
		touch-action: none;
	}

	.handle:hover,
	.handle:focus-visible {
		color: var(--text);
	}

	.check {
		padding-inline-end: 0;
		text-align: center;
	}

	input[type='checkbox'] {
		margin: 0;
		vertical-align: middle;
	}

	.name .cell {
		display: flex;
		align-items: center;
		gap: 0.55rem;
	}

	.kind {
		display: grid;
		flex: none;
		color: var(--muted);
	}

	.text {
		display: grid;
		min-width: 0;
	}

	.priority {
		display: grid;
		flex: none;
	}

	.priority.urgent {
		color: var(--warn);
	}

	.priority.high {
		color: var(--accent);
	}

	.priority.low {
		color: var(--faint);
	}

	.title {
		overflow: hidden;
		text-overflow: ellipsis;
		font-weight: 500;
	}

	/* The full description, only on phones (the columns say it otherwise). */
	.line {
		display: none;
	}

	.size,
	.speed,
	.left,
	.added {
		color: var(--muted);
		text-align: end;
	}

	.status,
	.category {
		color: var(--muted);
	}

	.failed .status {
		color: var(--danger);
	}

	.progress .cell {
		display: flex;
		align-items: center;
		gap: 0.5rem;
	}

	.percent {
		width: 2.6rem;
		color: var(--muted);
		font-size: 0.85em;
		text-align: end;
	}

	.actions {
		padding: 0 0.25rem;
		text-align: end;
	}

	.actions button {
		display: inline-grid;
		place-items: center;
		width: 1.9rem;
		height: 1.9rem;
		padding: 0;
		border-color: transparent;
		background: none;
		color: var(--muted);
		/* Shown on hover, focus or selection, so the list stays calm. */
		opacity: 0;
		border-radius: 50%;
	}

	tr:hover .actions button,
	tr.selected .actions button,
	.actions button:focus-visible {
		opacity: 1;
	}

	.actions button:hover {
		background: var(--surface);
		color: var(--text);
	}

	/* Touch screens have no hover: always show them. */
	@media (hover: none) {
		.actions button {
			opacity: 1;
		}
	}

	/* Less room: fewer columns (as JobTable's header says), kept at no width. */
	@container page (max-width: 72rem) {
		.category {
			width: 0;
			padding: 0;
			overflow: hidden;
			visibility: hidden;
			white-space: nowrap;
		}
	}

	@container page (max-width: 63rem) {
		.added {
			width: 0;
			padding: 0;
			overflow: hidden;
			visibility: hidden;
			white-space: nowrap;
		}
	}

	@container page (max-width: 53rem) {
		.size {
			width: 0;
			padding: 0;
			overflow: hidden;
			visibility: hidden;
			white-space: nowrap;
		}
	}

	/* Phones: name and description, then the bar; no columns. */
	@media (max-width: 40rem) {
		tr {
			display: grid;
			grid-template-columns: 2rem minmax(0, 1fr) auto;
			grid-template-areas: 'check name actions' '. progress progress';
			height: auto;
			padding: 0.5rem 0.25rem 0.6rem;
			border-bottom: 1px solid var(--border-soft);
		}

		td {
			border-bottom: none;
		}

		.check {
			grid-area: check;
			align-self: center;
		}

		.name {
			grid-area: name;
		}

		.line {
			display: block;
			overflow: hidden;
			text-overflow: ellipsis;
			color: var(--muted);
			font-size: 0.85em;
		}

		.failed .line {
			color: var(--danger);
		}

		.actions {
			grid-area: actions;
			align-self: center;
		}

		.progress {
			grid-area: progress;
			padding-top: 0.35rem;
		}

		.grip,
		.percent,
		.size,
		.category,
		.added,
		.status,
		.speed,
		.left {
			display: none;
		}
	}

	/* Each row a rounded strip (on its cells:
	   a row can't round its own corners), the selected one raised. */
	tr:hover {
		background: none;
	}

	td {
		padding: 0 0.5rem;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-variant-numeric: tabular-nums;
		border-bottom: none;
	}

	tr:hover td {
		background: var(--hover);
	}

	tr.selected td {
		background: var(--selected);
	}

	td:first-child {
		border-start-start-radius: var(--radius-m);
		border-end-start-radius: var(--radius-m);
	}

	td:last-child {
		border-start-end-radius: var(--radius-m);
		border-end-end-radius: var(--radius-m);
	}
</style>
