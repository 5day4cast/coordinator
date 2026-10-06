// The game form's "Picks per entry": at most every weather category at every selected station.
// Empty takes them all; the server checks the number again.
function updatePicks(form) {
  const input = form.querySelector('input[name="number_of_values_per_entry"]');
  if (!input) return;
  const metrics = Number(input.dataset.metrics) || 3;
  const most = form.querySelectorAll('input[name="locations"]:checked').length * metrics;
  const note = form.querySelector('[data-picks-note]');
  if (most) {
    input.max = String(most);
    input.placeholder = `All ${most}`;
    if (note) note.textContent = `1 to ${most}. Leave empty for all ${most}: ${metrics} per selected station.`;
  } else {
    input.removeAttribute('max');
    input.placeholder = 'All';
    if (note) note.textContent = `Leave empty for all: ${metrics} per selected station.`;
  }
}
// Station cards and map selections both change the form; the map's own clicks send `change` too.
document.addEventListener('change', event => {
  const form = event.target.closest?.('#game-creation');
  if (form) updatePicks(form);
});
