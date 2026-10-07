// Runs last in the bundle, once the page has parsed. The WASM wallet is not
// loaded here: it loads when someone clicks Log in, Sign up or Pay.
function initApp() {
  setupTelemetry();
  setupModalCloseHandlers();
  setupThemeToggle();
  setupPage();
  setupEntryForm();
  setupPayoutModal();
  setupSatchel();

  const body = document.body;
  // Not on window: it holds the signer once someone logs in.
  const authManager = new AuthManager(body.dataset.apiBase, body.dataset.network);
  setupAuthModals(authManager);
  authManager.attachEventListeners();
  setupHtmxAuth();
  initPayouts(body.dataset.apiBase, body.dataset.oracleBase);

  authManager.followOtherTabs();
  // A remembered login loads the wallet; anyone else never downloads it.
  const restoring = authManager.restoreLogin();
  // An account page opened by its address asks for a login unless one is remembered.
  if (document.querySelector(".sign-in-required")) {
    restoring.then((restored) => {
      if (!restored) openAuthModal("loginModal");
    });
  }
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", initApp);
} else {
  initApp();
}
