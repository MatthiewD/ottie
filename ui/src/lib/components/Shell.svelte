<script lang="ts">
	import { onMount, untrack, type Snippet } from 'svelte';
	import type { AddJob } from '@ottie/api-types/AddJob';
	import type { AddedJobs } from '@ottie/api-types/AddedJobs';
	import type { Job } from '@ottie/api-types/Job';
	import { afterNavigate, goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import type { Settings } from '@ottie/api-types/Settings';
	import Eye from '@lucide/svelte/icons/eye';
	import HardDrive from '@lucide/svelte/icons/hard-drive';
	import { AddRequest } from '#lib/addrequest.svelte.js';
	import { appearance, setAppearance } from '#lib/appearance.svelte.js';
	import { confirmation, targets, type Action } from '#lib/bulk.js';
	import { provideApp } from '#lib/context.js';
	import { parseFilters, showsNewJobs, type Status } from '#lib/filters.js';
	import { fadeIn } from '#lib/moving.svelte.js';
	import { m } from '#lib/paraglide/messages.js';
	import { chosenLanguage, switchLanguage } from '#lib/languages.js';
	import { isLocale, locales } from '#lib/paraglide/runtime.js';
	import { Session, type Answer } from '#lib/session.svelte.js';
	import { anywhere, pageKey, pageText, pasted } from '#lib/shortcuts.js';
	import { syncSteps } from '#lib/sync.js';
	import { takeHandedLinks } from '#lib/token.js';
	import { followAddLinks } from '#lib/window.js';
	import BatchDialog from './BatchDialog.svelte';
	import ConfirmDialog from './ConfirmDialog.svelte';
	import NavMenu from './NavMenu.svelte';
	import Setup from './Setup.svelte';
	import ShortcutsDialog from './ShortcutsDialog.svelte';
	import { queueNow } from '#lib/slots.js';
	import SpeedChip from './SpeedChip.svelte';
	import TitleBar from './TitleBar.svelte';

	interface Props {
		token: string;
		/** The daemon refused the token. */
		onUnauthorized: () => void;
		/** The page. */
		children: Snippet;
	}

	interface Question {
		message: string;
		confirmLabel: string;
		answer: (yes: boolean) => void;
	}

	let { token, onUnauthorized, children }: Props = $props();
	// One session for every page, so moving between them keeps the live
	// connection. The token can't change under it.
	// svelte-ignore state_referenced_locally
	const session = new Session(token, onUnauthorized);
	let question: Question | null = $state(null);
	/** Links in the list for adding several, while it's open: copied
	 * elsewhere, sent by the desktop app's window (More…, docs/decisions/0043),
	 * or pasted on a page (0046). */
	let copied: string | null = $state(null);
	/** The list of keyboard shortcuts is open ("?"). */
	let helping = $state(false);
	const adding = new AddRequest();
	/** The tabs 1 to 5 go to, in NavMenu's order. */
	const TABS = [
		resolve('/'),
		resolve('/downloads'),
		resolve('/categories'),
		resolve('/schedule'),
		resolve('/settings')
	];
	let main: HTMLElement | undefined = $state();
	// Another tab's page fades in (docs/decisions/0048); a filter changing
	// the address doesn't count. (The page's own address, not the
	// navigation's: its `from` can lack one, and a callback that throws
	// turns the move into a full reload.)
	let shownPath = page.url.pathname;
	afterNavigate(() => {
		if (main && page.url.pathname !== shownPath) {
			fadeIn(main);
		}
		shownPath = page.url.pathname;
	});
	/** The first start (or the setup run again from Settings): the setup page
	 * replaces the app until it's done. */
	let settingUp = $derived(
		(session.settings?.setupDone === false && !session.readOnly) || session.settingUpAgain
	);

	// svelte-ignore state_referenced_locally
	provideApp({ session, token, run, ask, add, addMany, adding });

	// A device that follows the shared look and language takes them on
	// whenever they change, and shares its look while none is shared. (Only
	// the settings and the choice to follow are watched: a change made here is
	// on its way to the daemon and comes back as the shared one.)
	$effect(() => {
		const settings = session.settings;
		if (settings && appearance.synced) {
			untrack(() => void follow(settings));
		}
	});

	async function follow(settings: Settings) {
		const steps = syncSteps(appearance, chosenLanguage(), settings, locales);
		if (steps.look) {
			setAppearance(steps.look);
		}
		if (steps.share && !session.readOnly) {
			await session.changeSettings(steps.share);
		}
		if (steps.systemLanguage) {
			switchLanguage(null);
		} else if (steps.language && isLocale(steps.language)) {
			switchLanguage(steps.language);
		}
	}

	onMount(() => {
		session.start();
		return () => session.stop();
	});

	onMount(() => followAddLinks((text) => (copied = text)));
	// Links from the browser extension's "Download all links" (docs/decisions/0055).
	onMount(() => {
		const handed = takeHandedLinks();
		if (handed !== null && !session.readOnly) {
			copied = handed;
		}
	});

	async function add(job: AddJob): Promise<boolean> {
		const added = await session.add(job);
		if (added) {
			const later = job.startAt !== undefined && job.startAt > Date.now();
			await showNewJobs(later ? 'scheduled' : 'queued');
		}
		return added;
	}

	async function addMany(jobs: AddJob[]): Promise<Answer<AddedJobs>> {
		const answer = await session.addMany(jobs);
		if (answer.ok && answer.value.ids.some((id) => id !== null)) {
			await showNewJobs();
		}
		return answer;
	}

	/** On a Downloads view that would hide new downloads, shows everything instead. */
	async function showNewJobs(state: Status = 'queued') {
		const downloads = resolve('/downloads');
		const filters = parseFilters(page.url.searchParams);
		if (page.url.pathname === downloads && !showsNewJobs(filters, state)) {
			await goto(downloads, { replace: true, reset: false });
		}
	}

	/** Keys for every tab (docs/decisions/0046); Downloads has its own too. */
	function keydown(event: KeyboardEvent) {
		const press = settingUp ? null : pageKey(event);
		const command = press && anywhere(press);
		if (!command) {
			return;
		}
		event.preventDefault();
		if (command.kind === 'tab') {
			const href = TABS[command.index];
			if (href !== undefined && page.url.pathname !== href) {
				void goto(href);
			}
		} else if (command.kind === 'help') {
			helping = true;
		} else if (command.kind === 'focusAdd' && !session.readOnly) {
			void askAdd(null);
		}
	}

	/** A link pasted anywhere but in a field: one goes in the add bar, with
	 * its step open; several (or a sequence) open the list. */
	function paste(event: ClipboardEvent) {
		const text = settingUp || session.readOnly ? null : pageText(event);
		if (text === null) {
			return;
		}
		const found = pasted(text);
		if (found.kind === 'none') {
			return;
		}
		event.preventDefault();
		if (found.kind === 'several') {
			copied = text;
		} else {
			void askAdd(found.link);
		}
	}

	/** The add bar takes the link (or only the cursor); from a page without
	 * one, on Downloads. */
	async function askAdd(link: string | null) {
		if (page.url.pathname !== resolve('/') && page.url.pathname !== resolve('/downloads')) {
			await goto(resolve('/downloads'));
		}
		adding.ask(link);
	}

	async function run(action: Action, jobs: Job[]): Promise<void> {
		const chosen = targets(jobs, action);
		const message = confirmation(action, chosen);
		if (message !== null) {
			const confirmLabel = action === 'cancel' ? m.cancel_yes() : m.remove_yes();
			if (!(await ask(message, confirmLabel))) {
				return;
			}
		}
		await session.run(
			action,
			chosen.map((job) => job.id)
		);
	}

	async function setLimit(bytesPerSec: number): Promise<boolean> {
		const why = await session.changeSettings({ globalLimit: bytesPerSec });
		if (why !== null) {
			session.error = why;
		}
		return why === null;
	}

	function ask(message: string, confirmLabel: string): Promise<boolean> {
		return new Promise((resolve) => {
			question = {
				message,
				confirmLabel,
				answer: (yes) => {
					question = null;
					resolve(yes);
				}
			};
		});
	}
</script>

<svelte:window onkeydown={keydown} onpaste={paste} />

<div class="app">
	<TitleBar>
		{#snippet children(level)}
			{#if !settingUp}
				<NavMenu jobs={session.list} compact={level >= 1} />
			{/if}
			<span class="end" class:squeezed={level >= 4}>
				<SpeedChip
					brief={level >= 2}
					squeezed={level >= 4}
					speed={session.history.at(-1) ?? 0}
					limit={session.settings?.globalLimit ?? null}
					schedule={queueNow(session.slotList)}
					connection={session.connection}
					onLimit={setLimit}
					readOnly={session.readOnly}
				/>
			</span>
		{/snippet}
	</TitleBar>

	{#if settingUp}
		<Setup />
	{:else}
		<main bind:this={main}>
			{#if session.settings?.storageFailing}
				<p class="failing" role="alert">
					<HardDrive size={15} aria-hidden="true" />
					<span>
						{m.storage_failing()}
						<span class="detail" dir="auto">{session.settings.storageFailing}</span>
					</span>
				</p>
			{/if}
			{#if session.readOnly}
				<p class="looking">
					<Eye size={15} aria-hidden="true" />
					{m.read_only_note()}
				</p>
			{/if}
			{#if session.error}
				<div class="error" role="alert">
					<span>{session.error}</span>
					<button onclick={() => (session.error = null)} aria-label={m.dismiss()}>✕</button>
				</div>
			{/if}
			{@render children()}
		</main>
	{/if}
</div>

{#if copied !== null && !settingUp}
	<!-- Keyed: links sent again start the list again. -->
	{#key copied}
		<BatchDialog text={copied} onClose={() => (copied = null)} />
	{/key}
{/if}

{#if helping}
	<ShortcutsDialog onClose={() => (helping = false)} />
{/if}

{#if question}
	<ConfirmDialog
		message={question.message}
		confirmLabel={question.confirmLabel}
		onAnswer={question.answer}
	/>
{/if}

<style>
	/* The app fills the window; each page scrolls inside `main`. */
	.app {
		display: grid;
		/* minmax(0, …): wide content scrolls or shrinks rather than widening the page. */
		grid-template-columns: minmax(0, 1fr);
		grid-template-rows: auto minmax(0, 1fr);
		height: 100vh;
		height: 100dvh;
	}

	/* Pushes the speed to the end of the title bar. */
	.end {
		margin-inline-start: auto;
	}

	/* Still short of room once folded all the way: the speed shortens. */
	.end.squeezed {
		display: flex;
		min-width: 0;
	}

	main {
		display: flex;
		flex-direction: column;
		min-width: 0;
		min-height: 0;
		/* Pages adapt to the room they get, not to the window:
		   `@container page (…)` in their styles. */
		container: page / inline-size;
		margin: 0 0.5rem 0.5rem;
		padding-top: 0.75rem;
		overflow: hidden;
		border-radius: 1.6rem;
		background: var(--bg);
		box-shadow: inset 0 1px 0 rgb(255 255 255 / 4%);
	}

	/* A read-only device: said once, above every page. */
	.looking {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		margin: 0 0.75rem 0.5rem;
		padding: 0.45rem 0.9rem;
		border-radius: var(--radius-pill);
		background: var(--sunken);
		color: var(--muted);
		font-size: 0.85rem;
	}

	/* Saving to the database fails: until it works again. */
	.failing {
		display: flex;
		align-items: flex-start;
		gap: 0.5rem;
		margin: 0 0.75rem 0.5rem;
		padding: 0.5rem 0.9rem;
		border-radius: var(--radius-m);
		background: var(--danger-bg);
		color: var(--danger);
		font-size: 0.85rem;
	}

	.failing :global(svg) {
		flex: none;
		margin-top: 0.1rem;
	}

	.failing .detail {
		display: block;
		font-family: var(--mono);
		font-size: 0.8rem;
		opacity: 0.85;
	}

	.error {
		display: flex;
		justify-content: space-between;
		gap: 1rem;
		margin: 0.75rem 0.75rem 0;
		padding: 0.6rem 0.9rem;
		border-radius: var(--radius-s);
		background: var(--danger-bg);
		color: var(--danger);
	}

	.error button {
		padding: 0 0.4rem;
		border: none;
		background: none;
		color: inherit;
	}

	/* Phones: room for the tab bar along the bottom. */
	@media (max-width: 40rem) {
		main {
			padding-bottom: 3.7rem;
			margin: 0 0.25rem;
			border-radius: 1.2rem 1.2rem 0 0;
		}
	}
</style>
