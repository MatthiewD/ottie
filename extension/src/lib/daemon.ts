// The extension as a client of Ottie's HTTP API (docs/api/README.md), with
// the token it was given.

import type { AddJob } from '@ottie/api-types/AddJob';
import type { Added } from '@ottie/api-types/Added';
import type { ApiError } from '@ottie/api-types/ApiError';
import type { Settings } from '@ottie/api-types/Settings';
import type { Pairing } from './settings';

const BASE = '/api/v1';
/** Long enough for a daemon far away; a download waits on it. */
const TIMEOUT_MS = 10_000;

/** Ottie refused the token. */
export class Unauthorized extends Error {
	constructor() {
		super('The token was refused.');
	}
}

/** Nothing answered at the address (or not in time). */
export class Unreachable extends Error {
	constructor() {
		super('Ottie did not answer.');
	}
}

/** Any other refusal, with Ottie's code (`explain.ts` translates it). */
export class Refused extends Error {
	constructor(
		readonly status: number,
		readonly body: ApiError | null
	) {
		super(body?.error ?? `Ottie answered ${status}.`);
	}
}

async function request(
	pairing: Pairing,
	method: string,
	path: string,
	body?: unknown
): Promise<Response> {
	let response: Response;
	try {
		response = await fetch(`${pairing.address}${BASE}${path}`, {
			method,
			headers: {
				Authorization: `Bearer ${pairing.token}`,
				...(body === undefined ? {} : { 'Content-Type': 'application/json' })
			},
			body: body === undefined ? undefined : JSON.stringify(body),
			// Never the browser's cookies for Ottie's address.
			credentials: 'omit',
			signal: AbortSignal.timeout(TIMEOUT_MS)
		});
	} catch {
		throw new Unreachable();
	}
	if (response.status === 401) {
		throw new Unauthorized();
	}
	if (!response.ok) {
		const error = (await response.json().catch(() => null)) as ApiError | null;
		throw new Refused(response.status, error);
	}
	return response;
}

/** The job with a `requestId` of its own, kept for every try: a try that
 * got no answer may still have added it, and Ottie then answers the next
 * with the same download instead of adding another (docs/decisions/0060). */
export function withRequestId(job: AddJob): AddJob {
	return job.requestId === undefined ? { ...job, requestId: crypto.randomUUID() } : job;
}

/** Adds a download; its ID. */
export async function addJob(pairing: Pairing, job: AddJob): Promise<string> {
	const response = await request(pairing, 'POST', '/jobs', job);
	return ((await response.json()) as Added).id;
}

/** Takes a download off Ottie's list (stopping it). One already gone counts
 * as taken off: a try whose answer was lost may have done it. */
export async function removeJob(pairing: Pairing, id: string): Promise<void> {
	try {
		await request(pairing, 'DELETE', `/jobs/${encodeURIComponent(id)}`);
	} catch (error) {
		if (!(error instanceof Refused && error.body?.code === 'jobNotFound')) {
			throw error;
		}
	}
}

/** Ottie's settings: whether the token works, and its shared look and language. */
export async function daemonSettings(pairing: Pairing): Promise<Settings> {
	const response = await request(pairing, 'GET', '/settings');
	return (await response.json()) as Settings;
}
