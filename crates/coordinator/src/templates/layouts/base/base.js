// Runs last in the bundle, once the page has parsed. The WASM wallet is not
// loaded here: it loads when someone opens the log-in or sign-up dialog.
function initApp() {
  setupModalCloseHandlers();
  setupThemeToggle();
  setupPage();
  setupEntryForm();
  setupPayoutModal();

  const body = document.body;
  // Not on window: it holds the signer once someone logs in.
  const authManager = new AuthManager(body.dataset.apiBase, body.dataset.network);
  setupAuthModals(authManager);
  authManager.attachEventListeners();
  setupHtmxAuth();
  initPayouts(body.dataset.apiBase, body.dataset.oracleBase);

  // An account page opened by its address asks for a login straight away.
  if (document.querySelector(".sign-in-required")) openAuthModal("loginModal");
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", initApp);
} else {
  initApp();
}
