const assert = require("node:assert/strict");
const test = require("node:test");
const { loadBundle } = require("./bundle.cjs");

// The reader's zone, as the browser has it.
process.env.TZ = "America/New_York";

function time(datetime, local, zoned = false, since = undefined) {
  const dataset = zoned ? { local, zone: "" } : { local };
  if (since) dataset.since = since;
  return {
    textContent: "fallback UTC",
    title: "",
    dataset,
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


test("competition starts show a weekday and the local daylight-saving zone", () => {
  const summer = time("2026-10-04T17:00:00Z", "weekday", true);
  const winter = time("2026-11-08T18:00:00Z", "weekday", true);
  const { localizeTimes } = loadBundle(["shared/page.js"], { Date, console }, ["localizeTimes"]);
  localizeTimes({ querySelectorAll: () => [summer, winter] });
  assert.equal(summer.textContent.replace(/\s/g, " "), "Sun, 1:00 PM EDT");
  assert.equal(winter.textContent.replace(/\s/g, " "), "Sun, 1:00 PM EST");
  assert.ok(summer.title.includes("2026"));
});

test("a window's end on the next local day keeps its date", () => {
  // Oct 3 00:00–12:00 UTC is one UTC day, but 8 PM to 8 AM in New York.
  const overnight = time("2026-10-03T12:00:00Z", "time", true, "2026-10-03T00:00:00Z");
  const daytime = time("2026-10-03T22:00:00Z", "time", true, "2026-10-03T12:00:00Z");
  const { localizeTimes } = loadBundle(["shared/page.js"], { Date, console }, ["localizeTimes"]);
  localizeTimes({ querySelectorAll: () => [overnight, daytime] });
  assert.equal(overnight.textContent.replace(/\s/g, " "), "Oct 3, 8:00 AM EDT");
  assert.equal(daytime.textContent.replace(/\s/g, " "), "6:00 PM EDT");
});
