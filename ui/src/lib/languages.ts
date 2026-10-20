// The languages the page speaks, and which one it shows (docs/decisions/0041):
// the one chosen on this device, else the closest to the system's, else
// English.

import { getLocale, isLocale, locales, type Locale } from './paraglide/runtime.js';

/** Each language in its own words, as the picker lists them. */
export const NAMES: Record<Locale, string> = {
	en: 'English',
	fr: 'Français',
	de: 'Deutsch',
	'es-ES': 'Español (España)',
	'es-419': 'Español (Latinoamérica)',
	it: 'Italiano',
	nl: 'Nederlands',
	pl: 'Polski',
	'pt-PT': 'Português (Portugal)',
	'pt-BR': 'Português (Brasil)',
	sv: 'Svenska',
	ro: 'Română',
	cs: 'Čeština',
	hu: 'Magyar',
	da: 'Dansk',
	fi: 'Suomi',
	sk: 'Slovenčina',
	hr: 'Hrvatski',
	sl: 'Slovenščina',
	lt: 'Lietuvių',
	lv: 'Latviešu',
	et: 'Eesti',
	ga: 'Gaeilge',
	mt: 'Malti',
	el: 'Ελληνικά',
	bg: 'Български',
	uk: 'Українська',
	'zh-Hans': '简体中文',
	'zh-Hant': '繁體中文',
	ja: '日本語',
	ko: '한국어',
	id: 'Bahasa Indonesia',
	hi: 'हिन्दी',
	vi: 'Tiếng Việt',
	tr: 'Türkçe',
	sw: 'Kiswahili',
	am: 'አማርኛ',
	ha: 'Hausa',
	yo: 'Èdè Yorùbá',
	zu: 'isiZulu',
	ar: 'العربية',
	fa: 'فارسی',
	ur: 'اردو'
};

/** Checked by a native speaker; the others say so under the picker. */
export const REVIEWED: readonly Locale[] = ['en', 'fr'];

/** Stand-ins for the languages Chrome, Edge and the desktop app have no
 * `Intl` data for: there, numbers, units and dates would come out in the
 * system's language. Their country's English instead, in every browser, as
 * the tray writes them too (app/messages/). Arabic keeps its own, with the
 * digits most Arabic software uses today (123, not ١٢٣) in every browser. */
const INTL: Partial<Record<Locale, string>> = {
	ha: 'en-NG',
	yo: 'en-NG',
	zu: 'en-ZA',
	ar: 'ar-u-nu-latn'
};

/** The locale `Intl` writes numbers, units and dates in for `locale`. */
export function intlLocale(locale: Locale): string {
	return INTL[locale] ?? locale;
}

/** The languages written right to left: their pages are mirrored
 * (docs/decisions/0042). */
const RIGHT_TO_LEFT: readonly Locale[] = ['ar', 'fa', 'ur'];

/** Which way `locale` is written, for the page's `dir`. */
export function direction(locale: Locale): 'ltr' | 'rtl' {
	return RIGHT_TO_LEFT.includes(locale) ? 'rtl' : 'ltr';
}

/** Where people can help with a translation. */
export const HELP_TRANSLATE = 'https://github.com/MatthiewD/ottie/issues';

/** The languages, in the picker's order: by name, as people read them. */
export function sortedLocales(): Locale[] {
	const collator = new Intl.Collator('en');
	return [...locales].sort((a, b) => collator.compare(NAMES[a], NAMES[b]));
}

/** The closest of `known` to the languages the system asks for (in its
 * order of preference), or undefined if none is close. */
export function closestLanguage(
	wanted: readonly string[],
	known: readonly string[]
): string | undefined {
	for (const tag of wanted) {
		for (const candidate of standIns(tag)) {
			const found = known.find((own) => own.toLowerCase() === candidate.toLowerCase());
			if (found) {
				return found;
			}
		}
	}
	return undefined;
}

/** `tag` itself, then the languages that stand for it, closest first:
 * Latin American Spanish for `es-MX` (Spain's for `es` and `es-ES`),
 * Brazil's Portuguese for `pt-BR` (Portugal's for any other), traditional
 * Chinese for `zh-TW`, `zh-HK` or `zh-Hant`; then the bare language. */
function standIns(tag: string): string[] {
	const [first = '', ...rest] = tag.replace(/_/g, '-').split('-');
	const language = first.toLowerCase();
	const parts = rest.map((part) => part.toUpperCase());
	const region = parts.find((part) => /^([A-Z]{2}|\d{3})$/.test(part));
	const script = parts.find((part) => part.length === 4);
	const own = [tag];
	if (language === 'es') {
		const american = region !== undefined && region !== 'ES';
		own.push(...(american ? ['es-419', 'es-ES'] : ['es-ES', 'es-419']));
	} else if (language === 'pt') {
		own.push(...(region === 'BR' ? ['pt-BR', 'pt-PT'] : ['pt-PT', 'pt-BR']));
	} else if (language === 'zh') {
		const traditional = script === 'HANT' || ['TW', 'HK', 'MO'].includes(region ?? '');
		own.push(traditional ? 'zh-Hant' : 'zh-Hans');
	}
	own.push(language);
	return own;
}

/** The language the system asks for, of those the page has. */
export function systemLanguage(): Locale {
	const found = closestLanguage(navigator.languages, locales);
	return found !== undefined && isLocale(found) ? found : 'en';
}

const KEY = 'ottie.language';

/** The language chosen on this device; null: it follows the system. */
export function chosenLanguage(): Locale | null {
	try {
		const chosen = localStorage.getItem(KEY);
		return chosen !== null && isLocale(chosen) ? chosen : null;
	} catch {
		return null;
	}
}

/** Reloads the page, showing `shown`, when another window of it (a tab, the
 * desktop app's tray menu) chooses a language that changes it. Returns what
 * stops listening. */
export function followLanguage(shown: Locale): () => void {
	const changed = (event: StorageEvent) => {
		// `null`: the whole storage was cleared.
		if ((event.key === KEY || event.key === null) && getLocale() !== shown) {
			location.reload();
		}
	};
	addEventListener('storage', changed);
	return () => removeEventListener('storage', changed);
}

/** Chooses a language on this device (null: the system's), reloading the
 * page if it shows another one. (Not through Paraglide's `setLocale`: it
 * compares with `getLocale()`, which already reads the new choice.) */
export function switchLanguage(locale: Locale | null): void {
	const shown = getLocale();
	try {
		if (locale === null) {
			localStorage.removeItem(KEY);
		} else {
			localStorage.setItem(KEY, locale);
		}
	} catch {
		// Storage refused: the language lasts until the page reloads.
	}
	if ((locale ?? systemLanguage()) !== shown) {
		location.reload();
	}
}
