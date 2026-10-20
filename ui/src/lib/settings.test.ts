import { describe, expect, it } from 'vitest';
import {
	RANGES,
	fieldToLimit,
	limitToField,
	parseSeconds,
	parseWhole,
	unitLabel
} from './settings';

const KB = 1024;
const MB = 1024 * KB;
const GB = 1024 * MB;

describe('speed limits', () => {
	it('show in the largest unit they have one of, in the page’s language', () => {
		expect(limitToField(40 * MB, 'en')).toEqual({ value: '40', unit: 'MB' });
		expect(limitToField(1.5 * MB, 'fr')).toEqual({ value: '1,5', unit: 'MB' });
		expect(limitToField(300 * KB, 'en')).toEqual({ value: '300', unit: 'kB' });
		expect(limitToField(2 * GB, 'en')).toEqual({ value: '2', unit: 'GB' });
		expect(limitToField(100, 'en')).toEqual({ value: '0.1', unit: 'kB' });
		expect(limitToField(0, 'en')).toEqual({ value: '', unit: 'MB' });
	});

	it('are read in the unit chosen, with a point or a comma', () => {
		expect(fieldToLimit('40', 'MB')).toBe(40 * MB);
		expect(fieldToLimit(' 0,5 ', 'MB')).toBe(MB / 2);
		expect(fieldToLimit('300', 'kB')).toBe(300 * KB);
		expect(fieldToLimit('1.25', 'GB')).toBe(1.25 * GB);
	});

	it('stop at the most the daemon takes', () => {
		expect(fieldToLimit('1024', 'GB')).toBe(1024 * GB);
		expect(fieldToLimit('5000', 'GB')).toBe(1024 * GB);
	});

	it('refuse nothing, zero and nonsense', () => {
		for (const text of ['', '0', '-3', 'fast', '1e999']) {
			expect(fieldToLimit(text, 'MB'), text).toBeNull();
		}
	});

	it('name their units in each language', () => {
		expect(unitLabel('kB', 'en')).toBe('kB/s');
		expect(unitLabel('GB', 'fr')).toBe('Go/s');
	});
});

describe('parseWhole', () => {
	it('takes whole numbers within the range', () => {
		expect(parseWhole('3', RANGES.maxConcurrent)).toBe(3);
		expect(parseWhole('0', RANGES.maxRetries)).toBe(0);
	});

	it('refuses the rest', () => {
		for (const text of ['', '0', '51', '2.5', 'two']) {
			expect(parseWhole(text, RANGES.maxConcurrent), text).toBeNull();
		}
	});
});

describe('parseSeconds', () => {
	it('gives milliseconds', () => {
		expect(parseSeconds('2', RANGES.firstRetryWaitMs)).toBe(2000);
		expect(parseSeconds('1,5', RANGES.firstRetryWaitMs)).toBe(1500);
	});

	it('refuses waits out of range', () => {
		expect(parseSeconds('0', RANGES.firstRetryWaitMs)).toBeNull();
		expect(parseSeconds('7200', RANGES.firstRetryWaitMs)).toBeNull();
		expect(parseSeconds('', RANGES.firstRetryWaitMs)).toBeNull();
	});
});
