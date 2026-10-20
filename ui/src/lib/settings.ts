// Settings as people type them: speed limits in kB/s, MB/s or GB/s and waits
// in seconds in the fields, bytes per second and milliseconds for the daemon.

import { RANGES } from '@ottie/api-types/Ranges';
import { m } from './paraglide/messages.js';
import { intlLocale } from './languages';
import { getLocale, type Locale } from './paraglide/runtime.js';

export type SpeedUnit = 'kB' | 'MB' | 'GB';
export const SPEED_UNITS: SpeedUnit[] = ['kB', 'MB', 'GB'];
/** Counted in 1024s, as everywhere on the page (`format.ts`). */
const UNIT_BYTES: Record<SpeedUnit, number> = { kB: 1024, MB: 1024 ** 2, GB: 1024 ** 3 };

/** Switching the total limit on for the first time on this device. */
export const DEFAULT_LIMIT = 10 * UNIT_BYTES.MB;

/** Common limits, offered as buttons beside the field (and in the desktop
 * app's tray: `PRESETS` in `app/src/tray.rs`). */
export const LIMIT_PRESETS: number[] = [
	256 * UNIT_BYTES.kB,
	512 * UNIT_BYTES.kB,
	UNIT_BYTES.MB,
	2 * UNIT_BYTES.MB,
	5 * UNIT_BYTES.MB,
	10 * UNIT_BYTES.MB,
	20 * UNIT_BYTES.MB,
	50 * UNIT_BYTES.MB
];

/** What people may choose, as the daemon allows it (generated from
 * `crates/api/src/settings.rs`). */
export { RANGES };

/** Which last limit a limit control remembers: the total one, or the one
 * last given to a download. Kept apart, so setting one never changes the
 * other's. */
export type LimitMemory = 'total' | 'download';
const LAST_LIMIT_KEYS: Record<LimitMemory, string> = {
	total: 'ottie.lastLimit',
	download: 'ottie.lastDownloadLimit'
};

/** A number for a field: "40", "0.5" (French: "0,5"), two decimals at most. */
export function toField(value: number, locale: Locale = getLocale()): string {
	return new Intl.NumberFormat(intlLocale(locale), {
		maximumFractionDigits: 2,
		useGrouping: false
	}).format(value);
}

/** What was typed, with a point or a comma; null if it isn't a number. */
function parseNumber(text: string): number | null {
	const trimmed = text.trim().replace(',', '.');
	const number = trimmed === '' ? NaN : Number(trimmed);
	return Number.isFinite(number) ? number : null;
}

/** "kB/s" (French: "ko/s"). */
export function unitLabel(unit: SpeedUnit, locale: Locale = getLocale()): string {
	const options = { locale };
	return unit === 'kB'
		? m.unit_kbps({}, options)
		: unit === 'MB'
			? m.unit_mbps({}, options)
			: m.unit_gbps({}, options);
}

/** A limit for its field, in the largest unit it has at least one of
 * ("1.5" MB/s, "300" kB/s); 0 (no limit) is an empty field in MB/s. */
export function limitToField(
	bytesPerSec: number,
	locale: Locale = getLocale()
): { value: string; unit: SpeedUnit } {
	if (bytesPerSec <= 0) {
		return { value: '', unit: 'MB' };
	}
	const unit = [...SPEED_UNITS].reverse().find((u) => bytesPerSec >= UNIT_BYTES[u]) ?? 'kB';
	const value = Math.round((bytesPerSec / UNIT_BYTES[unit]) * 100) / 100;
	return { value: toField(value, locale), unit };
}

/** A limit typed in `unit`, in bytes per second, at most what the daemon
 * takes (1 TiB/s); null unless above zero. */
export function fieldToLimit(text: string, unit: SpeedUnit): number | null {
	const number = parseNumber(text);
	if (number === null || number <= 0) {
		return null;
	}
	return Math.min(RANGES.speedLimit.max, Math.max(1, Math.round(number * UNIT_BYTES[unit])));
}

/** A whole number typed, from `min` to `max`; null otherwise. */
export function parseWhole(text: string, range: { min: number; max: number }): number | null {
	const number = parseNumber(text);
	return number !== null && Number.isInteger(number) && number >= range.min && number <= range.max
		? number
		: null;
}

/** Seconds typed (decimals allowed), in milliseconds; null outside the
 * range (in milliseconds). */
export function parseSeconds(text: string, range: { min: number; max: number }): number | null {
	const number = parseNumber(text);
	const ms = number === null ? null : Math.round(number * 1000);
	return ms !== null && ms >= range.min && ms <= range.max ? ms : null;
}

/** The limit last used on this device, so switching it back on restores it. */
export function loadLastLimit(memory: LimitMemory = 'total'): number {
	try {
		const stored = Number(localStorage.getItem(LAST_LIMIT_KEYS[memory]));
		return Number.isSafeInteger(stored) && stored > 0 ? stored : DEFAULT_LIMIT;
	} catch {
		return DEFAULT_LIMIT; // Storage blocked.
	}
}

export function saveLastLimit(bytesPerSec: number, memory: LimitMemory = 'total'): void {
	try {
		localStorage.setItem(LAST_LIMIT_KEYS[memory], String(bytesPerSec));
	} catch {
		// Storage blocked: the default comes back next time.
	}
}
