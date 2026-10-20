import { describe, expect, it } from 'vitest';
import {
	appEntries,
	asRefusal,
	browserEntries,
	explainOthers,
	MAX_NAME,
	nameFor,
	opening,
	openingLink,
	parseLink,
	withOther
} from './others';

const HERE = 'http://127.0.0.1:24068';

describe('parseLink', () => {
	it('takes the root and the token of a sign-in link', () => {
		expect(parseLink(' https://nas.tail1234.ts.net/downloads?x=1#token=ab12 ')).toEqual({
			address: 'https://nas.tail1234.ts.net/',
			token: 'ab12'
		});
		expect(parseLink('http://10.8.0.1:24068')).toEqual({
			address: 'http://10.8.0.1:24068/',
			token: null
		});
	});

	it('refuses what isn’t a web address, or a spoiled token', () => {
		expect(parseLink('nas.example')).toBeNull();
		expect(parseLink('file:///etc/passwd')).toBeNull();
		expect(parseLink('javascript:alert(1)')).toBeNull();
		expect(parseLink('https://nas.example/#token=')).toBeNull();
		expect(parseLink('https://nas.example/#token=a"b')).toBeNull();
	});
});

describe('nameFor', () => {
	it('tidies the name, else takes the host', () => {
		expect(nameFor('  NAS\n', 'https://nas.example/')).toBe('NAS');
		expect(nameFor('', 'https://nas.example:8443/')).toBe('nas.example:8443');
		expect([...nameFor('é'.repeat(100), 'https://nas.example/')]).toHaveLength(MAX_NAME);
	});
});

describe('the switcher’s rows', () => {
	const others = [{ id: 3, name: 'NAS', address: 'https://nas.example/' }];

	it('in the app: this computer first, the one shown marked', () => {
		expect(appEntries({ shown: 3, others }, HERE, 'This computer')).toEqual([
			{ id: null, name: 'This computer', address: `${HERE}/`, current: false },
			{ id: 3, name: 'NAS', address: 'https://nas.example/', current: true }
		]);
		expect(appEntries({ shown: null, others: [] }, HERE, 'This computer')[0].current).toBe(true);
	});

	it('in a browser: this page’s own first, by its host', () => {
		const rows = browserEntries([{ ...others[0], token: 'secret' }], HERE);
		expect(rows).toEqual([
			{ id: null, name: '127.0.0.1:24068', address: `${HERE}/`, current: true },
			{ id: 3, name: 'NAS', address: 'https://nas.example/', current: false }
		]);
	});
});

describe('a browser’s list', () => {
	it('adds with a new ID, keeping the token until it’s opened', () => {
		const once = withOther([], 'NAS', 'https://nas.example/#token=ab', HERE);
		expect(once).toEqual([{ id: 1, name: 'NAS', address: 'https://nas.example/', token: 'ab' }]);
		const twice = withOther(once as never, '', 'http://10.8.0.1:24068/', HERE);
		expect(twice).toEqual([
			{ id: 1, name: 'NAS', address: 'https://nas.example/', token: 'ab' },
			{ id: 2, name: '10.8.0.1:24068', address: 'http://10.8.0.1:24068/' }
		]);
	});

	it('refuses this page’s own address and what isn’t a link', () => {
		expect(withOther([], 'Me', `${HERE}/#token=ab`, HERE)).toBe('here');
		expect(withOther([], 'Bad', 'nas', HERE)).toBe('badLink');
	});

	it('opens signed in while it has the token', () => {
		const other = { id: 1, name: 'NAS', address: 'https://nas.example/' };
		expect(openingLink({ ...other, token: 'ab' })).toBe('https://nas.example/#token=ab');
		expect(openingLink(other)).toBe('https://nas.example/');
	});

	it('opens from the list it’s given, and forgets the token it opened with', () => {
		const list = [
			{ id: 1, name: 'NAS', address: 'https://nas.example/', token: 'ab' },
			{ id: 2, name: 'Server', address: 'https://server.example/', token: 'cd' }
		];
		expect(opening(list, 1)).toEqual({
			link: 'https://nas.example/#token=ab',
			kept: [{ id: 1, name: 'NAS', address: 'https://nas.example/' }, list[1]]
		});
		expect(opening(list, 3)).toBeNull();
	});
});

describe('refusals', () => {
	it('reads the app’s codes, an unknown one as unreachable', () => {
		expect(asRefusal('keychain')).toBe('keychain');
		expect(asRefusal('someday')).toBe('unreachable');
		expect(asRefusal(new Error('x'))).toBe('unreachable');
	});

	it('says why in both languages', () => {
		expect(explainOthers('refused', 'en')).toMatch(/sign-in link/);
		expect(explainOthers('here', 'fr')).toMatch(/adresse/);
	});
});
