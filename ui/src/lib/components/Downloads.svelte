<script lang="ts">
	import { tick } from 'svelte';
	import type { Job } from '@ottie/api-types/Job';
	import type { JobChange } from '@ottie/api-types/JobChange';
	import type { View } from '@ottie/api-types/View';
	import { resolve } from '$app/paths';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import {
		arrange,
		groupNote,
		loadArrangement,
		saveArrangement,
		sortBy,
		type Arrangement
	} from '#lib/arrange.js';
	import { pauseSteps, resumeTargets, targets } from '#lib/bulk.js';
	import { useApp } from '#lib/context.js';
	import {
		NO_FILTERS,
		activeCount,
		filtersQuery,
		isActive,
		matches,
		parseFilters,
		type Filters
	} from '#lib/filters.js';
	import { currentSpeed } from '#lib/jobs.js';
	import { openProblem, toOpen } from '#lib/opening.js';
	import { resizes, swaps } from '#lib/moving.svelte.js';
	import { m } from '#lib/paraglide/messages.js';
	import { canReorder, type Reorder } from '#lib/queue.js';
	import { NOTHING, keepShown, selectAll, step, type Selection } from '#lib/selection.js';
	import { inDownloads, pageKey, type Command } from '#lib/shortcuts.js';
	import { isFinished } from '#lib/status.js';
	import { viewBody } from '#lib/views.js';
	import { canOpenFiles, openFile, showInFolder } from '#lib/window.js';
	import AddCard from './AddCard.svelte';
	import ConfirmDialog from './ConfirmDialog.svelte';
	import FilterPanel from './FilterPanel.svelte';
	import DetailsEmpty from './DetailsEmpty.svelte';
	import JobDetails from './JobDetails.svelte';
	import JobTable from './JobTable.svelte';
	import QuickFilters from './QuickFilters.svelte';
	import Toolbar from './Toolbar.svelte';
	import ViewDialog from './ViewDialog.svelte';

	const { session, run } = useApp();
	const downloads = resolve('/downloads');
	/** Any time: what `matches` gets when no filter is by date. */
	const EPOCH = new Date(0);

	// Read from the address; set at once when changed here, so the next click
	// builds on it even before the address has caught up.
	let filters = $derived(parseFilters(page.url.searchParams));
	let arrangement: Arrangement = $state(loadArrangement());
	let open = $state(false);
	let head: HTMLDivElement | undefined = $state();
	// The pills and the panel take each other's place smoothly (docs/decisions/0048).
	resizes(
		() => head,
		() => open
	);
	/** Bumped by "Reset filters" under an empty list, so an open panel starts
	 * over (it keeps what's typed in its name field otherwise). */
	let resets = $state(0);
	// Replaced as a whole on every change, never edited in place.
	let selection: Selection = $state.raw(NOTHING);
	/** The view being named: a new one (null) or one to rename. */
	let naming: { view: View | null } | null = $state(null);
	let deleting: View | null = $state(null);
	/** A download moved in the queue from the keyboard: kept in sight once
	 * the list shows it in its new place (its index there before the move). */
	let following: { id: string; index: number } | null = null;

	// To the minute: "today" and "the past 7 days" need no more, and the
	// list isn't filtered and sorted again every second.
	let minute = $derived(Math.floor(session.now / 60_000));
	let today = $derived(new Date(minute * 60_000));
	// Only a filter by date reads the clock: without one, the list isn't
	// filtered again every minute.
	let shown = $derived.by(() => {
		const now = filters.added === 'any' ? EPOCH : today;
		return session.list.filter((job) => matches(job, filters, now));
	});
	let sections = $derived(arrange(shown, arrangement));
	let order = $derived(sections.flatMap((section) => section.jobs.map((job) => job.id)));
	// Downloads that were removed or left the view drop out of the selection.
	let current = $derived(keepShown(selection, order));
	let selected = $derived(shown.filter((job) => current.ids.has(job.id)));
	let finished = $derived(shown.filter(isFinished));
	// Against every download, as the daemon moves them, not only those shown.
	let canUp = $derived(canReorder(session.list, selected, 'up'));
	let canDown = $derived(canReorder(session.list, selected, 'down'));
	// The panel under the list shows the one selected download.
	let picked = $derived(selected.length === 1 ? selected[0] : null);
	let pane: HTMLElement | undefined = $state();
	let list: ReturnType<typeof JobTable> | undefined = $state();
	// The details and their empty state take each other's place smoothly, as
	// do two downloads' details of different heights (docs/decisions/0048).
	swaps(
		() => pane?.querySelector<HTMLElement>(':scope > .details'),
		() => picked?.id ?? null
	);
	let detailsOpen = $state(true);
	// Categories with downloads get a quick filter.
	let used = $derived(
		session.categoryList.filter((category) =>
			session.list.some((job) => job.category === category.id)
		)
	);
	// Pause all and Resume all act on every download, not only those shown.
	let steps = $derived(pauseSteps(session.list));
	let resumable = $derived(resumeTargets(session.list));
	/** In the desktop app, a download's file and folder open (docs/decisions/0045). */
	const opens = canOpenFiles();
	// Apart from the speed, which changes every second: counting the
	// running ones goes through every download.
	let running = $derived(session.list.filter(isActive).length);
	let facts = $derived({
		speed: session.history.at(-1) ?? 0,
		running,
		slots: session.settings?.maxConcurrent ?? null
	});

	function setFilters(next: Filters) {
		filters = next;
		void goto(downloads + filtersQuery(next), { replace: true, reset: false });
	}

	/** Saves a change to one download's own settings; shows why if it can't. */
	async function changeJob(id: string, change: JobChange): Promise<boolean> {
		const why = await session.changeJob(id, change);
		if (why !== null) {
			session.error = why;
		}
		return why === null;
	}

	function setArrangement(next: Arrangement) {
		arrangement = next;
		saveArrangement(next);
	}

	/** Moves the selection in the queue (the waiting group shows it). */
	function reorder(to: Reorder) {
		void session.reorder(
			selected.map((job) => job.id),
			to
		);
	}

	/** Saves what's shown as a new view, or renames one (keeping what it shows). */
	async function saveView(view: View | null, name: string): Promise<string | null> {
		if (view !== null) {
			const { filters, arrangement } = view;
			return session.saveView(view.id, { name: name.trim(), filters, arrangement });
		}
		const why = await session.saveView(null, viewBody(name, filters, arrangement));
		if (why === null) {
			open = false; // Back to the pills, where the new one shows.
		}
		return why;
	}

	/** Opens a finished download's file, or says why it can't. */
	async function openJob(job: Job) {
		const why = await openFile(job.id);
		if (why !== null) {
			session.error = openProblem(why, job, false);
		}
	}

	/** Opens a download's folder with its file selected. */
	async function reveal(job: Job) {
		const why = await showInFolder(job.id);
		if (why !== null) {
			session.error = openProblem(why, job, true);
		}
	}

	/** The waiting downloads first, so pausing a running one doesn't start them. */
	async function pauseAll() {
		await session.run('pause', steps.waiting);
		await session.run('pause', steps.running);
	}

	/** The list's keys (docs/decisions/0046), as in a file manager: the
	 * arrows, Home and End, Ctrl+A, Escape, Space (pause or resume), Enter
	 * (opens the one finished download selected), Delete; Alt+arrows move
	 * in the queue; F shows the filters. */
	function keydown(event: KeyboardEvent) {
		const press = pageKey(event);
		const command = press && inDownloads(press);
		if (command && act(command)) {
			event.preventDefault();
		}
	}

	/** Does it; false if there was nothing to do (the key keeps its usual job). */
	function act(command: Command): boolean {
		// A read-only device only moves through the list.
		const changes = ['toggle', 'remove', 'queue'].includes(command.kind);
		if (changes && session.readOnly) {
			return false;
		}
		switch (command.kind) {
			case 'step':
				selection = step(current, order, command.to, command.extend);
				void bringIntoView(selection.cursor);
				return true;
			case 'selectAll':
				selection = selectAll(order);
				return true;
			case 'clear':
				selection = NOTHING;
				return current.ids.size > 0;
			case 'toggle': {
				// Pauses those that can; when none can, resumes.
				if (selected.length === 0) {
					return false;
				}
				void run(targets(selected, 'pause').length > 0 ? 'pause' : 'resume', selected);
				return true;
			}
			case 'open': {
				const job = opens ? toOpen(selected) : null;
				if (job) {
					void openJob(job);
				}
				return job !== null;
			}
			case 'remove':
				if (selected.length > 0) {
					void run('remove', selected);
				}
				return selected.length > 0;
			case 'queue': {
				if (!canReorder(session.list, selected, command.to)) {
					return false;
				}
				const id = current.cursor ?? selected[0]?.id;
				following = id === undefined ? null : { id, index: order.indexOf(id) };
				reorder(command.to);
				return true;
			}
			case 'filters':
				open = !open;
				return true;
			default:
				return false;
		}
	}

	/** Scrolls the list to the row the keys reached. A row's checkbox or
	 * button keeps no focus: Space then pauses rather than ticks it. */
	async function bringIntoView(id: string | null) {
		const focused = document.activeElement;
		if (focused instanceof HTMLElement && focused.closest('tr[data-id]')) {
			focused.blur();
		}
		await tick();
		if (id !== null) {
			// A long list draws only the rows in view: it scrolls to it first.
			await list?.reveal(id);
		}
	}

	// Once a download moved from the keyboard shows in its new place.
	$effect(() => {
		const moved = following;
		if (moved && order.indexOf(moved.id) !== moved.index) {
			following = null;
			void bringIntoView(moved.id);
		}
	});
</script>

<svelte:window onkeydown={keydown} />

<div class="page">
	{#if !session.readOnly}
		<div class="add"><AddCard /></div>
	{/if}

	<section class="pane" aria-label={m.nav_downloads()} bind:this={pane}>
		<div class="head" bind:this={head}>
			<Toolbar
				{selected}
				{finished}
				onRun={run}
				categories={session.categoryList}
				onMove={(category) =>
					session.move(
						selected.map((job) => job.id),
						category
					)}
				onReorder={reorder}
				{canUp}
				{canDown}
				shown={shown.length}
				total={session.list.length}
				active={activeCount(filters)}
				{open}
				onToggle={() => (open = !open)}
				canPauseAll={steps.waiting.length + steps.running.length > 0}
				canResumeAll={resumable.length > 0}
				onPauseAll={() => void pauseAll()}
				onResumeAll={() => void session.run('resume', resumable)}
				readOnly={session.readOnly}
			/>
			{#if open}
				{#key resets}
					<FilterPanel
						{filters}
						{arrangement}
						jobs={session.list}
						categories={session.categoryList}
						onFilters={setFilters}
						onArrange={setArrangement}
						onSaveView={session.readOnly ? null : () => (naming = { view: null })}
						onClose={() => (open = false)}
					/>
				{/key}
			{:else}
				<QuickFilters
					{filters}
					{arrangement}
					jobs={session.list}
					categories={used}
					views={session.viewList}
					now={today}
					onArrange={setArrangement}
					onRename={(view) => (naming = { view })}
					onDelete={(view) => (deleting = view)}
					readOnly={session.readOnly}
				/>
			{/if}
		</div>

		{#if session.list.length === 0 && session.connection === 'live'}
			<p class="empty">{session.readOnly ? m.empty_read_only() : m.empty()}</p>
		{:else if shown.length === 0 && session.connection === 'live'}
			<div class="empty">
				<p>{m.empty_filter()}</p>
				<button
					onclick={() => {
						setFilters(NO_FILTERS);
						resets += 1;
					}}>{m.fl_reset_filters()}</button
				>
			</div>
		{:else}
			<JobTable
				bind:this={list}
				{sections}
				{arrangement}
				selection={current}
				categories={session.categories}
				rates={session.rates}
				now={session.now}
				note={(group) => groupNote(group, facts)}
				onSelect={(chosen) => {
					selection = chosen;
					following = null;
				}}
				onRun={run}
				onOpen={opens ? openJob : null}
				onSort={(sort) => setArrangement(sortBy(arrangement, sort))}
				onReorder={(ids, to) => void session.reorder(ids, to)}
				readOnly={session.readOnly}
			/>
		{/if}

		{#if picked}
			{@const job = picked}
			<JobDetails
				{job}
				categories={session.categoryList}
				speed={currentSpeed(session.rates[job.id], session.now)}
				now={session.now}
				maxRetries={session.settings?.maxRetries ?? null}
				open={detailsOpen}
				onToggle={() => (detailsOpen = !detailsOpen)}
				onRun={(action) => run(action, [job])}
				onMove={(category) => session.move([job.id], category)}
				onChange={(change) => changeJob(job.id, change)}
				getLink={() => session.streamLink(job.id)}
				onOpen={opens ? () => void openJob(job) : null}
				onShowFolder={opens ? () => void reveal(job) : null}
				readOnly={session.readOnly}
			/>
		{:else}
			<DetailsEmpty
				count={selected.length}
				open={detailsOpen}
				onToggle={() => (detailsOpen = !detailsOpen)}
			/>
		{/if}
	</section>
</div>

{#if naming}
	{@const view = naming.view}
	<ViewDialog
		title={view ? m.view_rename_title() : m.view_save_title()}
		name={view?.name ?? ''}
		hint={view ? undefined : m.view_save_hint()}
		onSave={(name) => saveView(view, name)}
		onClose={() => (naming = null)}
	/>
{/if}

{#if deleting}
	{@const view = deleting}
	<ConfirmDialog
		message={m.view_delete_confirm({ name: view.name })}
		confirmLabel={m.view_delete_button()}
		onAnswer={(yes) => {
			deleting = null;
			if (yes) {
				void session.removeView(view.id);
			}
		}}
	/>
{/if}

<style>
	/* The list fills what's left of the page and scrolls inside it. */
	.page {
		display: flex;
		flex: 1;
		flex-direction: column;
		min-height: 0;
	}

	.add {
		padding: 0.25rem 1rem 0.75rem;
	}

	/* The quick actions, the filters and the list, on one card that fills
	   the rest of the page; the list scrolls inside it. */
	.pane {
		display: flex;
		flex: 1;
		flex-direction: column;
		min-height: 0;
		margin: 0 1rem 1rem;
		overflow: hidden;
		border: 1px solid var(--edge);
		border-radius: var(--radius-l);
		background: var(--surface);
	}

	.head {
		display: grid;
		gap: 0.5rem;
		padding: 0.6rem 0.75rem 0.75rem;
		border-bottom: none;
	}

	/* Fills the room the list would take, so the details stay at the
	   bottom whatever's shown above them. */
	.empty {
		flex: 1;
		margin: 4rem 0;
		padding: 0 1rem;
		text-align: center;
		color: var(--muted);
	}

	div.empty {
		display: grid;
		align-content: start;
		justify-items: center;
		gap: 0.75rem;
	}

	div.empty p {
		margin: 0;
	}

	/* Short windows and phones: the list keeps a height of its own (see
	   JobTable) and the details come under it; the page scrolls. */
	@media (max-height: 40rem), (max-width: 40rem) {
		.page {
			overflow: auto;
		}

		.pane {
			flex: none;
		}
	}

	@media (max-width: 40rem) {
		.add {
			padding: 0.25rem 0.5rem 0.5rem;
		}

		.pane {
			margin: 0 0.5rem 0.5rem;
		}
	}
</style>
