// The extension's background (a service worker in Chromium-based browsers,
// a background script in Firefox): takes the browser's downloads over and
// hands them to Ottie (docs/decisions/0055).
//
// A download is never dropped: the browser's goes on until Ottie has
// accepted it, and is only then cancelled and taken off the browser's list.
// If the browser finished it first, it keeps it and Ottie's is removed.

import type { AddJob } from '@ottie/api-types/AddJob';
import { m } from '#ui/paraglide/messages.js';
import { overwriteGetLocale } from '#ui/paraglide/runtime.js';
import { cookieHeader } from './lib/cookies';
import { addJob, removeJob, Unauthorized, Unreachable, withRequestId } from './lib/daemon';
import { onThisComputer, pairHere, startHere } from './lib/host';
import { loadLook, pageLanguage } from './lib/look';
import { batchAddress, linksInPage } from './lib/menus';
import { RecentPosts } from './lib/posts';
import { loadSettings, saveSettings, type Pairing } from './lib/settings';
import { settle, skipReason, type BrowserDownloads } from './lib/takeover';
import { isSchemeMessage, shadeFor, toolbarIcons } from './lib/toolbar';

const posts = new RecentPosts();

const downloads: BrowserDownloads = {
	cancel: (id) => chrome.downloads.cancel(id),
	state: async (id) => (await chrome.downloads.search({ id }))[0]?.state,
	erase: async (id) => {
		await chrome.downloads.erase({ id });
	}
};

chrome.webRequest.onBeforeRequest.addListener(
	(details) => {
		if (details.method === 'POST') {
			posts.note(details.url, Date.now());
		}
		return undefined;
	},
	{ urls: ['<all_urls>'], types: ['main_frame', 'sub_frame'] }
);

// Installed where Ottie is: it signs in by itself (docs/decisions/0055).
// Without Ottie's host, the options page asks for a sign-in link.
chrome.runtime.onInstalled.addListener(() => {
	void signInHere();
	void makeMenus();
});

// The toolbar icon's shade follows the browser's dark mode: Firefox picks it
// from the manifest; elsewhere the offscreen page says (lib/toolbar.ts).
void followScheme();

chrome.runtime.onMessage.addListener((message) => {
	if (isSchemeMessage(message)) {
		void chrome.action.setIcon({ path: toolbarIcons(shadeFor(message.dark)) });
	}
});

async function followScheme(): Promise<void> {
	if (chrome.offscreen === undefined) {
		return;
	}
	try {
		if (!(await chrome.offscreen.hasDocument())) {
			await chrome.offscreen.createDocument({
				url: 'offscreen.html',
				reasons: [chrome.offscreen.Reason.MATCH_MEDIA],
				justification: 'The toolbar icon follows the browser’s dark mode.'
			});
		}
	} catch (error) {
		// Another start created it meanwhile, or none can be: the black one stays.
		console.warn('The toolbar icon won’t follow dark mode:', error);
	}
}

// The menus' texts follow Ottie's shared language once it's known.
chrome.storage.onChanged.addListener((changes, area) => {
	if (area === 'local' && 'look' in changes) {
		void makeMenus();
	}
});

const MENU_ONE = 'ottie-one';
const MENU_ALL = 'ottie-all';

async function makeMenus(): Promise<void> {
	const shown = pageLanguage(await loadLook(), navigator.languages);
	overwriteGetLocale(() => shown);
	await chrome.contextMenus.removeAll();
	chrome.contextMenus.create({
		id: MENU_ONE,
		title: m.ext_menu_one(),
		contexts: ['link', 'image', 'video', 'audio']
	});
	chrome.contextMenus.create({
		id: MENU_ALL,
		title: m.ext_menu_all(),
		contexts: ['page', 'selection']
	});
}

chrome.contextMenus.onClicked.addListener((info, tab) => {
	if (info.menuItemId === MENU_ONE) {
		const url = info.linkUrl ?? info.srcUrl;
		if (url !== undefined) {
			void downloadOne(url, info.pageUrl, tab);
		}
	} else if (info.menuItemId === MENU_ALL && tab?.id !== undefined) {
		void downloadAll(tab.id, info.selectionText !== undefined);
	}
});

/** The signed-in pairing, signing in through Ottie's host if there's none;
 * null (and the options page opens) if that can't be done. */
async function signedIn(): Promise<Pairing | null> {
	const { pairing } = await loadSettings();
	if (pairing !== null) {
		return pairing;
	}
	try {
		const fresh = await pairHere();
		await saveSettings({ pairing: fresh });
		return fresh;
	} catch {
		await chrome.runtime.openOptionsPage();
		return null;
	}
}

/** One link, as a download the browser would have started; if Ottie can't
 * take it, the browser downloads it after all. */
async function downloadOne(url: string, page: string | undefined, tab?: chrome.tabs.Tab) {
	const pairing = await signedIn();
	if (pairing === null) {
		return;
	}
	const storeId = (tab as { cookieStoreId?: string } | undefined)?.cookieStoreId;
	try {
		await handOver(pairing, {
			url,
			cookie: (await cookiesFor(url, storeId)) ?? undefined,
			referrer: page,
			userAgent: navigator.userAgent
		});
	} catch (error) {
		console.warn('Ottie did not take a link; the browser downloads it:', error);
		await chrome.downloads.download({ url });
	}
}

/** Every link in the page (or the selection) in Ottie's list for adding
 * several, in a new tab: there, people choose which to add. */
async function downloadAll(tabId: number, selectedOnly: boolean) {
	const pairing = await signedIn();
	if (pairing === null) {
		return;
	}
	const [found] = await chrome.scripting.executeScript({
		target: { tabId },
		func: linksInPage,
		args: [selectedOnly]
	});
	const links = typeof found?.result === 'string' ? found.result : '';
	if (links !== '') {
		await chrome.tabs.create({ url: batchAddress(pairing.address, pairing.token, links) });
	}
}

async function signInHere(): Promise<void> {
	const settings = await loadSettings();
	if (settings.pairing !== null) {
		return;
	}
	try {
		await saveSettings({ pairing: await pairHere() });
	} catch {
		// No host here: a link it is, from the options page.
	}
}

chrome.downloads.onCreated.addListener((item) => {
	void takeOver(item);
});

async function takeOver(item: chrome.downloads.DownloadItem): Promise<void> {
	const settings = await loadSettings();
	const skip = skipReason(item, settings, (url) => posts.has(url, Date.now()));
	if (skip !== null || settings.pairing === null) {
		return;
	}
	let pairing = settings.pairing;
	let id: string;
	try {
		const job: AddJob = {
			url: item.url,
			cookie:
				(await cookiesFor(item.url, (item as { cookieStoreId?: string }).cookieStoreId)) ??
				undefined,
			referrer: item.referrer === '' ? undefined : item.referrer,
			userAgent: navigator.userAgent
		};
		[pairing, id] = await handOver(pairing, job);
	} catch (error) {
		// Unreachable, refused or signed out: the browser carries on.
		console.warn('Ottie did not take a download:', error);
		return;
	}
	await setBrowserUi(false);
	try {
		if ((await settle(downloads, item.id, () => removeJob(pairing, id))) === 'both') {
			console.warn('The browser and Ottie both kept this download:', item.url);
		}
	} finally {
		await setBrowserUi(true);
	}
}

/** Adds the job; where Ottie's host set this browser up, starts Ottie if
 * it doesn't answer, or signs in again if it refused the token, then tries
 * once more. The browser's download carries on meanwhile. */
async function handOver(pairing: Pairing, asked: AddJob): Promise<[Pairing, string]> {
	const job = withRequestId(asked);
	try {
		return [pairing, await addJob(pairing, job)];
	} catch (error) {
		if (error instanceof Unreachable && onThisComputer(pairing.address) && (await startHere())) {
			return [pairing, await addJob(pairing, job)];
		}
		if (error instanceof Unauthorized && pairing.byHost === true) {
			const fresh = await pairHere();
			await saveSettings({ pairing: fresh });
			return [fresh, await addJob(fresh, job)];
		}
		throw error;
	}
}

/** The cookies the browser would send with a link, from the store it came
 * from (Firefox's containers have their own). */
async function cookiesFor(url: string, storeId: string | undefined): Promise<string | null> {
	try {
		const cookies = await chrome.cookies.getAll({ url, storeId });
		return cookieHeader(cookies);
	} catch {
		return null;
	}
}

/** Chromium-based browsers only: hides their download bubble while one
 * is taken over, so it doesn't open for a download that leaves at once. */
async function setBrowserUi(enabled: boolean): Promise<void> {
	try {
		await chrome.downloads.setUiOptions?.({ enabled });
	} catch {
		// Firefox has no such thing; nothing to do.
	}
}
