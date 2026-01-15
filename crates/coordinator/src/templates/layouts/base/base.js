async function initApp() {
  try {
    await window.initWasm();

    const body = document.body;
    const API_BASE = body.dataset.apiBase;
    const NETWORK = body.dataset.network;

    const authManager = new window.AuthManager(API_BASE, NETWORK);
    window.authManager = authManager;

    window.setupModalCloseHandlers();
    window.setupAuthModals(authManager);
    authManager.attachEventListeners();
    window.setupHtmxAuth();
    window.setupNavbarBurger();
    window.setupThemeToggle();
  } catch (error) {
    console.error("Failed to initialize app:", error);
  }
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", initApp);
} else {
  initApp();
}
