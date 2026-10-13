// The A2 time rows (r4.s5.w1): `YYYY-MM-DD HH:mm` UTC with the marker as a
// suffix on the value (the top's Dispatch-2 reading), the date-only read the
// Library's rows use, and the date range. Every conversion is integer
// arithmetic on the string — no `Date`, no float, no `Number(`.

import { describe, expect, it } from "vitest";

import { EM_DASH } from "./numbers";
import { dateRange, dateUtc, timeUtc } from "./time";

describe("timeUtc — epoch-ms text and RFC3339, both saying UTC", () => {
  it("converts epoch-millisecond text to the minute", () => {
    expect(timeUtc("1738368900000")).toBe("2025-02-01 00:15 UTC");
    expect(timeUtc("1735691400000")).toBe("2025-01-01 00:30 UTC");
  });

  it("renders an RFC3339 instant floored to the minute", () => {
    expect(timeUtc("2026-10-04T00:44:59.999Z")).toBe("2026-10-04 00:44 UTC");
    expect(timeUtc("2026-09-02T10:15:30.123Z")).toBe("2026-09-02 10:15 UTC");
  });

  it("accepts the explicit zero offset as UTC", () => {
    expect(timeUtc("2026-09-02T10:15:30+00:00")).toBe("2026-09-02 10:15 UTC");
  });

  it("shows an instant in a non-UTC offset as given, never shifted", () => {
    expect(timeUtc("2026-10-04T02:44:59+02:00")).toBe("2026-10-04T02:44:59+02:00");
  });

  it("crosses month and year boundaries in the civil-date step", () => {
    expect(timeUtc("1767225600000")).toBe("2026-01-01 00:00 UTC");
    expect(timeUtc("1893456000000")).toBe("2030-01-01 00:00 UTC");
  });
});

describe("dateUtc — the date-only read for the Library's rows", () => {
  it("prints the civil date and the marker", () => {
    expect(dateUtc("2026-08-21T08:30:00.000Z")).toBe("2026-08-21 UTC");
    expect(dateUtc("1735689600000")).toBe("2025-01-01 UTC");
  });
});

describe("dateRange — YYYY-MM-DD → YYYY-MM-DD, one marker for the pair", () => {
  it("renders both sides with the marker once", () => {
    expect(dateRange("2026-01-01T00:00:00.000Z", "2026-09-01T00:00:00.000Z")).toBe(
      "2026-01-01 → 2026-09-01 UTC",
    );
    expect(dateRange("1735689600000", "1756684800000")).toBe("2025-01-01 → 2025-09-01 UTC");
  });

  it("renders the em dash alone when either side is absent", () => {
    expect(dateRange(null, "2026-09-01T00:00:00.000Z")).toBe(EM_DASH);
    expect(dateRange("2026-01-01T00:00:00.000Z", null)).toBe(EM_DASH);
  });
});

describe("null, absent and malformed input", () => {
  it("renders the em dash for null and blank", () => {
    expect(timeUtc(null)).toBe(EM_DASH);
    expect(timeUtc("")).toBe(EM_DASH);
    expect(dateUtc(undefined)).toBe(EM_DASH);
  });

  it("shows unreadable text as given", () => {
    expect(timeUtc("not a time")).toBe("not a time");
    expect(timeUtc("2026-13-40T99:99:99Z")).toBe("2026-13-40T99:99:99Z");
  });
});
