const assert = require("node:assert/strict");
const test = require("node:test");
const { loadBundle } = require("./bundle.cjs");

// The reader's zone, as the browser has it.
process.env.TZ = "America/New_York";

function time(datetime, local, zoned = false) {
  return {
    textContent: "fallback UTC",
    title: "",
    dataset: zoned ? { local, zone: "" } : { local },
    getAttribute: (name) => (assert.equal(name, "datetime"), datetime),
  };
}

test("a window's end names the reader's zone once; other times don't", () => {
  const start = time("2026-09-30T02:02:00Z", "datetime");
  const end = time("2026-10-01T02:02:00Z", "datetime", true);
  const sameDay = time("2026-09-30T14:00:00Z", "time", true);
  const root = { querySelectorAll: () => [start, end, sameDay] };
  const { localizeTimes } = loadBundle(["shared/page.js"], { Date, console }, ["localizeTimes"]);
  localizeTimes(root);
  assert.equal(start.textContent.replace(/\s/g, " "), "Sep 29, 10:02 PM");
  assert.equal(end.textContent.replace(/\s/g, " "), "Sep 30, 10:02 PM EDT");
  assert.equal(sameDay.textContent.replace(/\s/g, " "), "10:00 AM EDT");
});
