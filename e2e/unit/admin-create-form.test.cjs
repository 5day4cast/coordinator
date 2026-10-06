const assert = require("node:assert/strict");
const test = require("node:test");
const { loadBundle } = require("./bundle.cjs");

// The admin game form's picks field, with `checked` stations selected.
function gameForm(checked, metrics = "3") {
  const input = {
    dataset: { metrics },
    max: undefined,
    placeholder: "All",
    removeAttribute(name) { this[name] = undefined; },
  };
  const note = { textContent: "" };
  const form = {
    querySelector: (selector) => ({ 'input[name="number_of_values_per_entry"]': input, "[data-picks-note]": note })[selector],
    querySelectorAll: (selector) => {
      assert.equal(selector, 'input[name="locations"]:checked');
      return Array.from({ length: checked });
    },
  };
  return { form, input, note };
}

function load() {
  let listener;
  const document = { addEventListener: (type, handler) => { assert.equal(type, "change"); listener = handler; } };
  const { updatePicks } = loadBundle(["admin/create_form.js"], { document }, ["updatePicks"]);
  return { updatePicks, change: (event) => listener(event) };
}

test("picks per entry allow up to every category at every selected station", () => {
  const { updatePicks } = load();
  const { form, input, note } = gameForm(4);
  updatePicks(form);
  assert.equal(input.max, "12");
  assert.equal(input.placeholder, "All 12");
  assert.equal(note.textContent, "1 to 12. Leave empty for all 12: 3 per selected station.");

  const half = gameForm(3, "2");
  updatePicks(half.form);
  assert.equal(half.input.max, "6");
});

test("with no station selected there is no maximum yet", () => {
  const { change } = load();
  const { form, input, note } = gameForm(0);
  input.max = "9";
  change({ target: { closest: (selector) => (selector === "#game-creation" ? form : null) } });
  assert.equal(input.max, undefined);
  assert.equal(input.placeholder, "All");
  assert.equal(note.textContent, "Leave empty for all: 3 per selected station.");
});

test("changes outside the game form are left alone", () => {
  const { change } = load();
  change({ target: { closest: () => null } });
  change({ target: {} });
});
