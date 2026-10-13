// The A2 display rules (r4.s5.w1, spine r4.s5) — one owner for every money,
// price, quantity, R, percent and fold figure the Mac app renders.
//
// Every function takes the server's exact decimal text and returns display
// text. The arithmetic is string arithmetic on the digits the server sent —
// never `Number`, `parseFloat` or `toFixed` (A1: the UI computes no money; the
// only multiplication allowed is the fraction-to-percent point shift, and it
// is done by moving the decimal point). The server's exact text stays
// reachable separately, as the element's `title`/`data-exact` (`Figure.tsx`).
//
// Rounding is half away from zero on the decimal text. Thousands separators
// are `,`. The rendered minus is U+2212. `null`, `undefined` and a blank string
// render the em dash; text that is not a decimal is shown as given, so a
// server change never blanks a screen.

/** The em dash every absent value renders. */
export const EM_DASH = "—";

/** The rendered minus — U+2212, one glyph for every signed figure. */
const MINUS = "−";

// The sign class keeps `+`, the ASCII hyphen and U+2212 apart from the digits
// (the hyphen last, so `+-` is not read as a range).
const DECIMAL = /^([+−-]?)(\d+)(?:\.(\d+))?$/;

interface Decimal {
  /** `""`, `"-"` or `"+"` — as the server sent it, normalised. */
  readonly sign: "" | "-" | "+";
  /** The integer digits, leading zeros kept (they are stripped for display). */
  readonly int: string;
  /** The fraction digits, exactly as sent (may be empty). */
  readonly frac: string;
}

/** The text as a decimal, or `null` when it is not one. */
function asDecimal(text: string): Decimal | null {
  const match = DECIMAL.exec(text);
  if (match === null) return null;
  const sign = match[1] === "-" || match[1] === MINUS ? "-" : match[1] === "+" ? "+" : "";
  return { sign, int: match[2], frac: match[3] ?? "" };
}

/** Whether a raw input is absent rather than malformed. */
function isAbsent(raw: string | null | undefined): raw is null | undefined {
  return raw === null || raw === undefined || raw.trim() === "";
}

/** The digits with the leading zeros stripped, `"0"` when nothing is left. */
function plainInt(int: string): string {
  const stripped = int.replace(/^0+/, "");
  return stripped === "" ? "0" : stripped;
}

/** `digits` + 1, as a digit string (one digit longer when it carries out). */
function increment(digits: string): string {
  let out = "";
  let carry = 1;
  for (let i = digits.length - 1; i >= 0; i -= 1) {
    const next = digits.charCodeAt(i) - 48 + carry;
    out = String(next % 10) + out;
    carry = next >= 10 ? 1 : 0;
  }
  return carry === 1 ? "1" + out : out;
}

interface Rounded {
  readonly int: string;
  readonly frac: string;
  readonly zero: boolean;
}

/** The magnitude rounded to `places` digits after the point, half away from
 * zero, on the digit string. */
function round(dec: Decimal, places: number): Rounded {
  const digits = dec.int + dec.frac;
  const kept = Math.max(dec.int.length + places, 0);
  let out = digits.slice(0, kept).padEnd(kept, "0");
  if ((digits.charAt(kept) || "0") >= "5") out = increment(out);
  const int = out.slice(0, out.length - places) || "0";
  const frac = places === 0 ? "" : out.slice(out.length - places);
  const zero = plainInt(int) === "0" && /^0*$/.test(frac);
  return { int, frac, zero };
}

/** The sign a figure renders: the server's own minus; a `+` only where the
 * sign is the reader's signal; and nothing at all for a zero magnitude. */
function renderSign(sign: Decimal["sign"], signed: boolean, zero: boolean): string {
  if (zero) return "";
  if (sign === "-") return MINUS;
  return signed ? "+" : "";
}

/** The integer part with thousands separators. */
function group(int: string): string {
  const plain = plainInt(int);
  let out = "";
  for (let i = plain.length; i > 0; i -= 3) {
    const start = Math.max(0, i - 3);
    out = out === "" ? plain.slice(start, i) : `${plain.slice(start, i)},${out}`;
  }
  return out;
}

/** USDT amount — 2 decimals, thousands separators, the unit where the column
 * header does not carry it. `signed` adds the `+` a P&L-type figure shows. */
export function usdt(
  raw: string | null | undefined,
  options: { signed?: boolean; unit?: boolean } = {},
): string {
  if (isAbsent(raw)) return EM_DASH;
  const dec = asDecimal(raw.trim());
  if (dec === null) return raw.trim();
  const rounded = round(dec, 2);
  const body = `${group(rounded.int)}.${rounded.frac}`;
  const sign = renderSign(dec.sign, options.signed === true, rounded.zero);
  return `${sign}${body}${options.unit === false ? "" : " USDT"}`;
}

/** Price — 2 decimals at ≥ 1,000; 4 at ≥ 1; 6 below 1; thousands separators. */
export function price(raw: string | null | undefined): string {
  if (isAbsent(raw)) return EM_DASH;
  const dec = asDecimal(raw.trim());
  if (dec === null) return raw.trim();
  const magnitude = plainInt(dec.int);
  const places = magnitude.length >= 4 ? 2 : magnitude === "0" ? 6 : 4;
  const rounded = round(dec, places);
  return `${renderSign(dec.sign, false, rounded.zero)}${group(rounded.int)}.${rounded.frac}`;
}

/** Quantity — up to 4 decimals, trailing zeros trimmed. */
export function qty(raw: string | null | undefined): string {
  if (isAbsent(raw)) return EM_DASH;
  const dec = asDecimal(raw.trim());
  if (dec === null) return raw.trim();
  const rounded = round(dec, 4);
  const frac = rounded.frac.replace(/0+$/, "");
  const body = frac === "" ? plainInt(rounded.int) : `${plainInt(rounded.int)}.${frac}`;
  return `${renderSign(dec.sign, false, rounded.zero)}${body}`;
}

/** R multiple — 2 decimals, the `R` suffix, always a sign. Accepts the ring's
 * own pre-formatted shape (`"+0.42R"`) and normalises it. */
export function rMultiple(raw: string | null | undefined): string {
  if (isAbsent(raw)) return EM_DASH;
  const stripped = raw.trim().replace(/\s*[Rr]$/, "").trim();
  const dec = asDecimal(stripped);
  if (dec === null) return raw.trim();
  const rounded = round(dec, 2);
  const sign = renderSign(dec.sign, true, rounded.zero);
  return `${sign}${plainInt(rounded.int)}.${rounded.frac} R`;
}

/** A ratio as percent — the fraction's point moved two places on the string,
 * one decimal, `%`. A text that is already a percent is passed through. */
export function percentFromFraction(raw: string | null | undefined): string {
  if (isAbsent(raw)) return EM_DASH;
  const text = raw.trim();
  if (text.endsWith("%")) return text;
  const dec = asDecimal(text);
  if (dec === null) return text;
  // The one allowed multiplication, done as the point moving: the digits keep
  // their order and the point shifts two places to the right.
  const shifted = dec.int + dec.frac;
  const int = shifted.slice(0, dec.int.length + 2).padEnd(dec.int.length + 2, "0");
  const frac = shifted.slice(dec.int.length + 2);
  const rounded = round({ sign: dec.sign, int, frac }, 1);
  const sign = renderSign(dec.sign, false, rounded.zero);
  return `${sign}${plainInt(rounded.int)}.${rounded.frac}%`;
}

/** The other-decimals row — `places` decimals, no separators, no added sign. */
export function decimal(raw: string | null | undefined, places: number): string {
  if (isAbsent(raw)) return EM_DASH;
  const dec = asDecimal(raw.trim());
  if (dec === null) return raw.trim();
  const rounded = round(dec, places);
  return `${renderSign(dec.sign, false, rounded.zero)}${plainInt(rounded.int)}.${rounded.frac}`;
}

/** The walk-forward folds sentence (#328, #305): the scheme's count, not a
 * bare ratio. */
export function foldsHold(pass: number, total: number, required: number): string {
  return `${pass} of ${total} folds hold (${required} required)`;
}
