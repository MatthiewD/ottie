// The daemon's codes in the page's language. The daemon also sends English
// text, used for codes this UI doesn't know (a newer daemon) and as detail.

import type { ApiError } from '@ottie/api-types/ApiError';
import type { Problem } from '@ottie/api-types/Problem';
import { formatFingerprint } from './certificates';
import { formatBytes, formatCount, formatDuration } from './format';
import { m } from './paraglide/messages.js';
import { getLocale, type Locale } from './paraglide/runtime.js';

/** Why a download failed or is retrying. */
export function explainProblem(
	problem: Problem | null,
	english: string | null,
	locale: Locale = getLocale()
): string {
	const options = { locale };
	if (!problem) {
		return english ?? '';
	}
	switch (problem.code) {
		case 'network':
			return m.problem_network({ detail: problem.detail }, options);
		case 'redirect':
			return m.problem_redirect({ detail: problem.detail }, options);
		case 'httpStatus':
			return httpStatus(problem.status, locale);
		case 'stalled':
			return m.problem_stalled({ time: formatDuration(problem.seconds, locale) }, options);
		case 'incomplete':
			return m.problem_incomplete(
				{
					received: formatBytes(problem.received, locale),
					expected: formatBytes(problem.expected, locale)
				},
				options
			);
		case 'remoteChanging':
			return m.problem_remote_changing({}, options);
		case 'disk':
			return m.problem_disk({ detail: problem.detail }, options);
		case 'fileExists':
			return m.problem_file_exists({ name: problem.name }, options);
		case 'ftpReply':
			return ftpReply(problem.status, problem.detail, locale);
		case 'certificate':
			return m.problem_certificate(
				{ host: problem.host, fingerprint: formatFingerprint(problem.fingerprint) },
				options
			);
		case 'noTls':
			return m.problem_no_tls({}, options);
		case 'other':
			return problem.detail;
		default:
			return english ?? '';
	}
}

function httpStatus(status: number, locale: Locale): string {
	const options = { locale };
	switch (status) {
		case 403:
			return m.problem_http_403({}, options);
		case 404:
			return m.problem_http_404({}, options);
		case 410:
			return m.problem_http_410({}, options);
		case 429:
			return m.problem_http_429({}, options);
		default:
			return status >= 500
				? m.problem_http_5xx({ status: String(status) }, options)
				: m.problem_http({ status: String(status) }, options);
	}
}

/** An FTP server's refusal: the usual ones in words, else its code and
 * what it said. */
function ftpReply(status: number, detail: string, locale: Locale): string {
	const options = { locale };
	switch (status) {
		case 421:
			return m.problem_ftp_421({}, options);
		case 530:
			return m.problem_ftp_530({}, options);
		case 550:
			return m.problem_ftp_550({}, options);
		default:
			return m.problem_ftp({ status: String(status), detail }, options);
	}
}

/** Why the daemon refused a request. `fallback` is used without a body. */
export function explainRefusal(
	body: ApiError | null,
	fallback: string,
	locale: Locale = getLocale()
): string {
	const options = { locale };
	if (!body) {
		return fallback;
	}
	switch (body.code) {
		case 'invalidUrl':
			return m.reason_invalid_url({}, options);
		case 'unsupportedScheme':
			return m.reason_unsupported_scheme({ scheme: body.scheme }, options);
		case 'relativeFolder':
			return m.reason_relative_folder({}, options);
		case 'nonUtf8Path':
			return m.reason_non_utf8_path({}, options);
		case 'noFolder':
			return m.reason_no_folder({ category: body.category }, options);
		case 'jobNotFound':
			return m.reason_job_not_found({}, options);
		case 'invalidTransition':
			return m.reason_invalid_transition({}, options);
		case 'stopBeforeStart':
			return m.reason_stop_before_start({}, options);
		case 'slotNotFound':
			return m.reason_slot_not_found({}, options);
		case 'slotWithoutDays':
			return m.reason_slot_without_days({}, options);
		case 'slotWithoutEffect':
			return m.reason_slot_without_effect({}, options);
		case 'invalidTime':
			return m.reason_invalid_time({ text: body.text }, options);
		case 'invalidSlotOrder':
			return m.reason_invalid_slot_order({}, options);
		case 'stopping':
			return m.reason_stopping({}, options);
		case 'storageFailed':
			return m.reason_storage_failed({}, options);
		case 'categoryNotFound':
			return m.reason_category_not_found({}, options);
		case 'invalidCategoryName':
			return m.reason_invalid_category_name({}, options);
		case 'invalidExtension':
			return m.reason_invalid_extension({ extension: body.extension }, options);
		case 'categoryNameTaken':
			return m.reason_category_name_taken({ name: body.name }, options);
		case 'extensionTaken':
			return m.reason_extension_taken(
				{ extension: body.extension, category: body.category },
				options
			);
		case 'cannotRemoveBuiltIn':
			return m.reason_cannot_remove_built_in({ category: body.category }, options);
		case 'outOfRange':
			return outOfRange(body.setting, body.min, body.max, locale);
		case 'invalidLanguage':
			return m.reason_invalid_language({ language: body.language }, options);
		case 'jobMoving':
			return m.reason_job_moving({}, options);
		case 'cannotMoveWhileRunning':
			return m.reason_cannot_move_while_running({}, options);
		case 'moveFailed':
			return m.reason_move_failed({ detail: body.detail }, options);
		case 'viewNotFound':
			return m.reason_view_not_found({}, options);
		case 'invalidViewName':
			return m.reason_invalid_view_name({}, options);
		case 'viewNameTaken':
			return m.reason_view_name_taken({ name: body.name }, options);
		case 'invalidCertificate':
			return m.reason_invalid_certificate({}, options);
		case 'certificateNotFound':
			return m.reason_certificate_not_found({}, options);
		case 'readOnly':
			return m.reason_read_only({}, options);
		case 'ownerOnly':
			return m.reason_owner_only({}, options);
		case 'folderNotAllowed':
			return m.reason_folder_not_allowed({ folder: body.folder }, options);
		case 'invalidDeviceName':
			return m.reason_invalid_device_name({}, options);
		case 'deviceNotFound':
			return m.reason_device_not_found({}, options);
		case 'tooManyLinks':
			return m.reason_too_many_links(
				{ count: formatCount(body.count, locale), max: formatCount(body.max, locale) },
				options
			);
		default:
			// Codes for screens this UI doesn't have yet, internal errors, and
			// codes from a newer daemon.
			return body.error;
	}
}

/** The settings the daemon checks, by their field names. */
const SETTING_NAMES: Record<string, typeof m.set_max_concurrent> = {
	maxConcurrent: m.set_max_concurrent,
	maxRetries: m.set_max_retries,
	retriesBeforeYield: m.set_yield,
	maxBatch: m.set_max_batch
};

function outOfRange(setting: string, min: number, max: number, locale: Locale): string {
	const options = { locale };
	if (setting === 'firstRetryWaitMs') {
		const [from, to] = [min, max].map((ms) => formatCount(ms / 1000, locale));
		return m.reason_out_of_range_seconds(
			{ setting: m.set_first_wait({}, options), min: from, max: to },
			options
		);
	}
	const name = SETTING_NAMES[setting]?.({}, options) ?? setting;
	const [from, to] = [min, max].map((value) => formatCount(value, locale));
	return m.reason_out_of_range({ setting: name, min: from, max: to }, options);
}
