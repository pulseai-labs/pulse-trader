// The A2 format rules (r4.s5.w1) — every row of the grill's table, over the
// server's exact decimal text. The module is pure string arithmetic: these
// tests pin the display text, and the screens' own tests pin the exact text
// that stays reachable through `Figure`'s `title`/`data-exact`.

import { describe, expect, it } from "vitest";

import {
  EM_DASH,
  decimal,
  foldsHold,
  percentFromFraction,
  price,
  qty,
  rMultiple,
  usdt,
} from "./numbers";

describe("usdt — 2 decimals, separators, the sign where it is the reader's signal", () => {
  it("rounds the server's long decimal text to two places and groups the thousands", () => {
    expect(usdt("1234.5612345678901234567", { signed: true })).toBe("+1,234.56 USDT");
  });

  it("rounds half away from zero", () => {
    expect(usdt("0.005")).toBe("0.01 USDT");
    expect(usdt("-0.005")).toBe("−0.01 USDT");
    expect(usdt("1234.565", { signed: true })).toBe("+1,234.57 USDT");
  });

  it("keeps the server's own minus as U+2212 and never adds a second sign", () => {
    expect(usdt("-12.30")).toBe("−12.30 USDT");
    expect(usdt("−12.30", { signed: true })).toBe("−12.30 USDT");
  });

  it("renders a magnitude that rounds to zero unsigned, never −0.00", () => {
    expect(usdt("-0.004")).toBe("0.00 USDT");
    expect(usdt("-0.004", { signed: true })).toBe("0.00 USDT");
    expect(usdt("0", { signed: true })).toBe("0.00 USDT");
  });

  it("omits the unit where the column header carries it", () => {
    expect(usdt("12.5", { signed: true, unit: false })).toBe("+12.50");
  });

  it("adds no sign unless asked", () => {
    expect(usdt("412.750")).toBe("412.75 USDT");
  });

  it("carries a long integer part through the rounding and the grouping", () => {
    expect(usdt("12345678901234567890123456789.987654321", { signed: true })).toBe(
      "+12,345,678,901,234,567,890,123,456,789.99 USDT",
    );
  });
});

describe("price — 2 decimals at ≥ 1,000; 4 at ≥ 1; 6 below 1", () => {
  it("renders each magnitude band at its own precision", () => {
    expect(price("64250.50")).toBe("64,250.50");
    expect(price("59900")).toBe("59,900.00");
    expect(price("1.5")).toBe("1.5000");
    expect(price("0.5")).toBe("0.500000");
    expect(price("0.1234567")).toBe("0.123457");
  });

  it("carries the rounding into the integer part", () => {
    expect(price("9999.995")).toBe("10,000.00");
  });

  it("keeps the server's sign and adds none", () => {
    expect(price("-0.0000005")).toBe("−0.000001");
  });
});

describe("qty — up to 4 decimals, trailing zeros trimmed", () => {
  it("trims the server's padded zeros", () => {
    expect(qty("0.01500000")).toBe("0.015");
    expect(qty("2.50000")).toBe("2.5");
    expect(qty("3.0000")).toBe("3");
  });

  it("rounds to the fourth decimal and trims what that leaves", () => {
    expect(qty("0.00001")).toBe("0");
  });
});

describe("rMultiple — 2 decimals, the R suffix, always a sign", () => {
  it("normalises the ring's pre-formatted shape", () => {
    expect(rMultiple("+0.42R")).toBe("+0.42 R");
  });

  it("signs both directions and rounds at the half", () => {
    expect(rMultiple("1.290")).toBe("+1.29 R");
    expect(rMultiple("-1.658")).toBe("−1.66 R");
    expect(rMultiple("0.005")).toBe("+0.01 R");
    expect(rMultiple("-0.005")).toBe("−0.01 R");
  });

  it("renders a magnitude that rounds to zero unsigned", () => {
    expect(rMultiple("-0.004")).toBe("0.00 R");
  });
});

describe("percentFromFraction — the point shift on the string, one decimal", () => {
  it("shifts a fraction two places and rounds to one decimal", () => {
    expect(percentFromFraction("0.4375")).toBe("43.8%");
    expect(percentFromFraction("0.416666")).toBe("41.7%");
    expect(percentFromFraction("0.5")).toBe("50.0%");
  });

  it("treats an already-percent text as shifted, never double-shifting it", () => {
    expect(percentFromFraction("48.3%")).toBe("48.3%");
  });

  it("handles zero, one and a negative fraction", () => {
    expect(percentFromFraction("0")).toBe("0.0%");
    expect(percentFromFraction("1")).toBe("100.0%");
    expect(percentFromFraction("-0.05")).toBe("−5.0%");
  });
});

describe("decimal — the other-decimals row", () => {
  it("keeps the requested places, half away from zero", () => {
    expect(decimal("0.681", 2)).toBe("0.68");
    expect(decimal("-0.8123", 2)).toBe("−0.81");
    expect(decimal("0.181", 3)).toBe("0.181");
    expect(decimal("2.638", 2)).toBe("2.64");
    expect(decimal("12", 2)).toBe("12.00");
  });
});

describe("foldsHold — pass of total folds hold (required required)", () => {
  it("states the scheme's count, not a bare ratio", () => {
    expect(foldsHold(0, 4, 4)).toBe("0 of 4 folds hold (4 required)");
    expect(foldsHold(6, 6, 4)).toBe("6 of 6 folds hold (4 required)");
    expect(foldsHold(3, 6, 4)).toBe("3 of 6 folds hold (4 required)");
  });
});

describe("null, absent and malformed input", () => {
  it("renders the em dash for null, undefined and a blank string", () => {
    expect(usdt(null)).toBe(EM_DASH);
    expect(price(undefined)).toBe(EM_DASH);
    expect(qty("")).toBe(EM_DASH);
    expect(rMultiple("   ")).toBe(EM_DASH);
    expect(percentFromFraction(null)).toBe(EM_DASH);
    expect(decimal(null, 2)).toBe(EM_DASH);
  });

  it("shows malformed text as given — a server change never blanks a screen", () => {
    expect(usdt("n/a")).toBe("n/a");
    expect(price("1.2.3")).toBe("1.2.3");
    expect(decimal("abc", 2)).toBe("abc");
  });
});
