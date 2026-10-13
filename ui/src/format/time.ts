// The A2 time rows (r4.s5.w1): `YYYY-MM-DD HH:mm` UTC with the marker as a
// suffix on the value, the date-only read the Library's rows use, and the
// `YYYY-MM-DD → YYYY-MM-DD` range.
//
// The conversion is integer arithmetic on the string: epoch-millisecond text
// goes through `BigInt` and the civil-from-days algorithm, and an RFC 3339
// instant is printed from its own fields, floored to the minute. No `Date`,
// no float, no timezone lookup — the same instant renders the same text in
// every process. An instant carrying a non-UTC offset is shown as given,
// never silently shifted.

import { EM_DASH } from "./numbers";

/** The marker every rendered time carries (the spec's own example,
 * `2026-10-09 14:15 UTC`). */
const UTC = " UTC";

const EPOCH_MS = /^\d+$/;
const RFC3339 =
  /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2})(?:\.\d+)?)?(Z|\+00:00)$/;

interface Instant {
  readonly year: string;
  readonly month: string;
  readonly day: string;
  readonly hour: string;
  readonly minute: string;
}

function isAbsent(raw: string | null | undefined): raw is null | undefined {
  return raw === null || raw === undefined || raw.trim() === "";
}

function pad(value: bigint, width: number): string {
  return value.toString().padStart(width, "0");
}

/** `civil_from_days` (Howard Hinnant's algorithm), in `BigInt` — integer
 * arithmetic, no float, correct for every instant this app renders. */
function civilFromMinutes(minutes: bigint): Instant {
  const days = minutes / 1440n;
  const minuteOfDay = minutes % 1440n;
  const z = days + 719468n;
  const era = (z >= 0n ? z : z - 146096n) / 146097n;
  const doe = z - era * 146097n;
  const yoe = (doe - doe / 1460n + doe / 36524n - doe / 146096n) / 365n;
  const y = yoe + era * 400n;
  const doy = doe - (365n * yoe + yoe / 4n - yoe / 100n);
  const mp = (5n * doy + 2n) / 153n;
  const d = doy - (153n * mp + 2n) / 5n + 1n;
  const m = mp + (mp < 10n ? 3n : -9n);
  return {
    year: pad(m <= 2n ? y + 1n : y, 4),
    month: pad(m, 2),
    day: pad(d, 2),
    hour: pad(minuteOfDay / 60n, 2),
    minute: pad(minuteOfDay % 60n, 2),
  };
}

/** The instant an input text names, or `null` when it names none this module
 * can print as UTC. */
function instantOf(raw: string): Instant | null {
  const text = raw.trim();
  if (EPOCH_MS.test(text)) {
    return civilFromMinutes(BigInt(text) / 60000n);
  }
  const match = RFC3339.exec(text);
  if (match === null) return null;
  const [, month, day, hour, minute] = match.slice(1, 6).map(Number);
  if (
    month < 1 || month > 12 ||
    day < 1 || day > 31 ||
    hour > 23 || minute > 59
  ) {
    return null;
  }
  return {
    year: match[1],
    month: match[2],
    day: match[3],
    hour: match[4],
    minute: match[5],
  };
}

/** `YYYY-MM-DD HH:mm` UTC from epoch-millisecond text or an RFC 3339 instant
 * (floored to the minute). Unreadable text is shown as given. */
export function timeUtc(raw: string | null | undefined): string {
  if (isAbsent(raw)) return EM_DASH;
  const instant = instantOf(raw);
  if (instant === null) return raw.trim();
  return `${instant.year}-${instant.month}-${instant.day} ${instant.hour}:${instant.minute}${UTC}`;
}

/** `YYYY-MM-DD` UTC — the date-only read the Library's rows and the date range
 * use. */
export function dateUtc(raw: string | null | undefined): string {
  if (isAbsent(raw)) return EM_DASH;
  const instant = instantOf(raw);
  if (instant === null) return raw.trim();
  return `${instant.year}-${instant.month}-${instant.day}${UTC}`;
}

/** `YYYY-MM-DD → YYYY-MM-DD` UTC — one marker for the pair. Either side
 * absent renders the em dash alone; an unreadable side is shown as given. */
export function dateRange(
  from: string | null | undefined,
  to: string | null | undefined,
): string {
  if (isAbsent(from) || isAbsent(to)) return EM_DASH;
  const start = dateUtc(from);
  const end = dateUtc(to);
  if (start === EM_DASH || end === EM_DASH) return EM_DASH;
  if (!start.endsWith(UTC) || !end.endsWith(UTC)) {
    // At least one side is not an instant this module reads: show the pair as
    // given rather than claim a zone for it.
    return `${from.trim()} → ${to.trim()}`;
  }
  return `${start.slice(0, -UTC.length)} → ${end.slice(0, -UTC.length)}${UTC}`;
}
