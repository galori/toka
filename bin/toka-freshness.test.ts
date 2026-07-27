import { formatAge } from "./toka-freshness.mjs";

test("formats build age in useful units", () => {
  expect(formatAge(12)).toBe("12 minutes");
  expect(formatAge(125)).toBe("2 hours (125 minutes)");
  expect(formatAge(2_880)).toBe("2 days (48 hours)");
});
