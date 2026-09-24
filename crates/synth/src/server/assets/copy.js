// Copy buttons: `data-copy` holds the text itself; `data-copy-url` names an export to fetch and
// copy, so pages do not carry every export inline. The page works without this; it only saves
// selecting the text.
document.addEventListener('click', (event) => {
  const button = event.target.closest('[data-copy], [data-copy-url]');
  if (!button || !navigator.clipboard) return;
  const url = button.dataset.copyUrl;
  const text = url
    ? fetch(url, { credentials: 'same-origin' }).then((response) => {
        if (!response.ok) throw new Error(`${url}: ${response.status}`);
        return response.text();
      })
    : Promise.resolve(button.dataset.copy);
  // A ClipboardItem takes the pending text, so the copy stays tied to the click that asked
  // for it while the export is fetched.
  const copied = typeof ClipboardItem === 'function' && url
    ? navigator.clipboard.write([
        new ClipboardItem({ 'text/plain': text.then((t) => new Blob([t], { type: 'text/plain' })) }),
      ])
    : text.then((t) => navigator.clipboard.writeText(t));
  const label = button.textContent;
  copied.then(
    () => { button.textContent = 'copied'; },
    () => { button.textContent = 'copy failed'; },
  ).then(() => setTimeout(() => { button.textContent = label; }, 1200));
});
