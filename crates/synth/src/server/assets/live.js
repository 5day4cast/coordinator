// Swaps in each fragment pushed for the page's topic, keeping open details open, and posts the
// action buttons without leaving the page.
(() => {
  const live = document.getElementById('live');
  const status = document.getElementById('live-status');
  const say = (text) => { if (status) status.textContent = text; };
  const source = new EventSource('/api/live?topic=' + encodeURIComponent(live.dataset.topic));
  source.onopen = () => say('● live');
  source.onerror = () => say('○ reconnecting…');
  source.addEventListener('live', (event) => {
    const open = new Set([...live.querySelectorAll('details[open]')].map((d) => d.dataset.key));
    live.innerHTML = event.data;
    live.querySelectorAll('details').forEach((d) => { if (open.has(d.dataset.key)) d.open = true; });
  });
  document.addEventListener('submit', async (event) => {
    const form = event.target;
    if (!form.matches('form[data-async]')) return;
    event.preventDefault();
    const button = form.querySelector('button');
    if (button) button.disabled = true;
    try {
      await fetch(form.action, { method: 'POST' });
    } finally {
      if (button) button.disabled = false;
    }
  });
})();
