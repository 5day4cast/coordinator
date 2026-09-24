// Runs last in the bundle, once the page has parsed. The WASM wallet is not
// loaded here: it loads when someone opens the log-in or sign-up dialog.
function initApp() {
  window.setupModalCloseHandlers?.();
  window.setupNavbarBurger?.();
  window.setupThemeToggle?.();
  window.setupPage?.();
  window.setupEntryForm?.();
  window.setupPayoutModal?.();

  const body = document.body;
  // Not on window: it holds the signer once someone logs in.
  const authManager = new window.AuthManager(body.dataset.apiBase, body.dataset.network);
  window.setupAuthModals(authManager);
  authManager.attachEventListeners();
  window.setupHtmxAuth?.();
  window.initPayouts?.(body.dataset.apiBase, body.dataset.oracleBase);

  // An account page opened by its address asks for a login straight away.
  if (document.querySelector(".sign-in-required")) window.openAuthModal?.("loginModal");
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", initApp);
} else {
  initApp();
}
