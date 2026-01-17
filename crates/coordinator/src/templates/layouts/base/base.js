async function initApp() {
  // Setup non-WASM dependent features first (UI interactions, theme, navigation)
  window.setupModalCloseHandlers?.();
  window.setupNavbarBurger?.();
  window.setupThemeToggle?.();

  try {
    await window.initWasm();

    const body = document.body;
    const API_BASE = body.dataset.apiBase;
    const NETWORK = body.dataset.network;

    const authManager = new window.AuthManager(API_BASE, NETWORK);
    window.authManager = authManager;

    window.setupAuthModals(authManager);
    authManager.attachEventListeners();
    window.setupHtmxAuth?.();
  } catch (error) {
    console.error("Failed to initialize WASM:", error);
    // Auth features won't work, but basic UI will still function
  }
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", initApp);
} else {
  initApp();
}
