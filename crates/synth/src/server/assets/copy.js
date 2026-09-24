// Copy buttons: `data-copy` holds the text itself, `data-copy-from` names the element whose
// value or text to copy. The page works without this; it only saves selecting the text.
document.addEventListener('click', (event) => {
  const button = event.target.closest('[data-copy], [data-copy-from]');
  if (!button || !navigator.clipboard) return;
  const source = button.dataset.copyFrom && document.querySelector(button.dataset.copyFrom);
  const text = button.dataset.copy ?? (source ? source.value ?? source.textContent : '');
  navigator.clipboard.writeText(text).then(() => {
    const label = button.textContent;
    button.textContent = 'copied';
    setTimeout(() => { button.textContent = label; }, 1200);
  });
});
