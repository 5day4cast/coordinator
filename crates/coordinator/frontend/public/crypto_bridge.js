/**
 * Crypto Bridge for HTMX
 *
 * Bridges the gap between HTMX (hypermedia-driven) and WASM (crypto operations).
 * Injects Nostr NIP-98 authentication headers into HTMX requests
 * for routes that require authentication.
 */

import init, {
  NostrClientWrapper,
  TaprootWallet,
  TaprootWalletBuilder,
  SignerType,
} from "./dist/client_validator.js";

import { AuthorizedClient } from "./authorized_client.js";

// Routes that require authentication
const AUTH_REQUIRED_ROUTES = ["/entries", "/payouts"];

// Global state
let authManager = null;

/**
 * Check if a URL requires authentication
 */
function requiresAuth(url) {
  return AUTH_REQUIRED_ROUTES.some((route) => url.includes(route));
}

/**
 * Check if user is logged in (nostrClient is initialized with keys)
 */
function isLoggedIn() {
  return window.nostrClient && window.taprootWallet;
}

/**
 * Generate a Nostr NIP-98 auth header for the given request
 */
async function generateAuthHeader(method, url) {
  if (!isLoggedIn()) {
    return null;
  }

  try {
    // Get the full URL for the auth event
    const fullUrl = new URL(url, window.location.origin).href;

    // Generate the auth header using the WASM nostrClient
    const header = await window.nostrClient.getAuthHeader(
      fullUrl,
      method,
      null,
    );
    return header;
  } catch (error) {
    console.error("Failed to generate auth header:", error);
    return null;
  }
}

/**
 * Initialize the application
 */
export async function initApp() {
  // Initialize WASM
  await init();

  // Create a new (empty) NostrClientWrapper
  window.nostrClient = new NostrClientWrapper();

  // Create auth manager
  authManager = new AuthManager(API_BASE, NETWORK);
  window.authManager = authManager;

  // Setup modal handlers
  setupModalHandlers();

  // Setup HTMX auth interception
  setupHtmxAuth();

  console.log("App initialized");
}

/**
 * Setup HTMX authentication header injection
 */
function setupHtmxAuth() {
  // Intercept requests that need auth
  document.body.addEventListener("htmx:configRequest", async (event) => {
    const { verb, path } = event.detail;

    if (!requiresAuth(path)) {
      return;
    }

    // Check if user is logged in
    if (!isLoggedIn()) {
      // Let the request proceed - server will return HTML login prompt
      return;
    }

    // Generate and attach auth header
    const authHeader = await generateAuthHeader(verb, path);
    if (authHeader) {
      event.detail.headers["Authorization"] = authHeader;
    }
  });
}

/**
 * Setup modal open/close handlers
 */
function setupModalHandlers() {
  // Modal utility functions
  function openModal($el) {
    $el.classList.add("is-active");
    document.documentElement.classList.add("is-clipped");
  }

  function closeModal($el) {
    $el.classList.remove("is-active");
    document.documentElement.classList.remove("is-clipped");
  }

  function closeAllModals() {
    document.querySelectorAll(".modal").forEach(($modal) => {
      closeModal($modal);
    });
  }

  function resetLoginModal() {
    const privateKeyError = document.querySelector("#privateKeyError");
    if (privateKeyError) privateKeyError.textContent = "";

    const extensionError = document.querySelector("#extensionLoginError");
    if (extensionError) extensionError.textContent = "";

    const loginInput = document.getElementById("loginPrivateKey");
    if (loginInput) loginInput.value = "";

    const privateKeyTab = document.querySelector(
      "#loginModal .tabs li[data-target='privateKeyLogin']",
    );
    if (privateKeyTab) privateKeyTab.click();
  }

  function resetRegisterModal() {
    const extensionError = document.querySelector("#extensionRegisterError");
    if (extensionError) extensionError.textContent = "";

    const display = document.getElementById("privateKeyDisplay");
    if (display) display.value = "";

    const checkbox = document.getElementById("privateKeySavedCheckbox");
    if (checkbox) checkbox.checked = false;

    const button = document.getElementById("registerStep1Button");
    if (button) button.disabled = true;

    const step1 = document.getElementById("registerStep1");
    const step2 = document.getElementById("registerStep2");
    if (step1) step1.classList.remove("is-hidden");
    if (step2) step2.classList.add("is-hidden");

    const privateKeyTab = document.querySelector(
      "#registerModal .tabs li[data-target='registerPrivateKey']",
    );
    if (privateKeyTab) privateKeyTab.click();
  }

  // Login button click
  const loginNavClick = document.getElementById("loginNavClick");
  if (loginNavClick) {
    loginNavClick.addEventListener("click", () => {
      resetLoginModal();
      openModal(document.getElementById("loginModal"));
    });
  }

  // Register button click
  const registerNavClick = document.getElementById("registerNavClick");
  if (registerNavClick) {
    registerNavClick.addEventListener("click", () => {
      resetRegisterModal();
      openModal(document.getElementById("registerModal"));
      if (authManager) authManager.handleRegisterStep1();
    });
  }

  // Close login modal
  const closeLoginModal = document.getElementById("closeLoginModal");
  if (closeLoginModal) {
    closeLoginModal.addEventListener("click", () => {
      closeModal(document.getElementById("loginModal"));
    });
  }

  // Close register modal
  const closeRegisterModal = document.getElementById("closeResisterModal");
  if (closeRegisterModal) {
    closeRegisterModal.addEventListener("click", () => {
      resetRegisterModal();
      closeModal(document.getElementById("registerModal"));
    });
  }

  // Show register from login
  const showRegisterButton = document.getElementById("showRegisterButton");
  if (showRegisterButton) {
    showRegisterButton.addEventListener("click", () => {
      closeModal(document.getElementById("loginModal"));
      resetRegisterModal();
      openModal(document.getElementById("registerModal"));
      if (authManager) authManager.handleRegisterStep1();
    });
  }

  // Go to login from register
  const goToLoginButton = document.getElementById("goToLoginButton");
  if (goToLoginButton) {
    goToLoginButton.addEventListener("click", () => {
      closeModal(document.getElementById("registerModal"));
      openModal(document.getElementById("loginModal"));
    });
  }

  // Modal background clicks close modal
  document
    .querySelectorAll(
      ".modal-background, .modal-close, .modal-card-head .delete, .modal-card-foot .button.is-cancel",
    )
    .forEach(($close) => {
      const $target = $close.closest(".modal");
      $close.addEventListener("click", () => {
        closeModal($target);
      });
    });

  // Escape key closes all modals
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      closeAllModals();
    }
  });

  // Export close function for use by auth manager
  window.closeAllModals = closeAllModals;
}

/**
 * Auth Manager - handles login/registration with WASM
 */
class AuthManager {
  constructor(apiBase, network) {
    this.apiBase = apiBase;
    this.network = network;
    this.authorizedClient = null;
    this.attachEventListeners();
  }

  attachEventListeners() {
    // Login buttons
    const loginButton = document.getElementById("loginButton");
    if (loginButton) {
      loginButton.addEventListener("click", () => this.handlePrivateKeyLogin());
    }

    const extensionLoginButton = document.getElementById(
      "extensionLoginButton",
    );
    if (extensionLoginButton) {
      extensionLoginButton.addEventListener("click", () =>
        this.handleExtensionLogin(),
      );
    }

    // Register buttons
    const registerStep1Button = document.getElementById("registerStep1Button");
    if (registerStep1Button) {
      registerStep1Button.addEventListener("click", () =>
        this.handleRegistrationComplete(),
      );
    }

    const extensionRegisterButton = document.getElementById(
      "extensionRegisterButton",
    );
    if (extensionRegisterButton) {
      extensionRegisterButton.addEventListener("click", () =>
        this.handleExtensionRegistration(),
      );
    }

    // Checkbox to enable next button
    const checkbox = document.getElementById("privateKeySavedCheckbox");
    if (checkbox) {
      checkbox.addEventListener("change", (e) => {
        const btn = document.getElementById("registerStep1Button");
        if (btn) btn.disabled = !e.target.checked;
      });
    }

    // Copy private key
    const copyButton = document.getElementById("copyPrivateKey");
    if (copyButton) {
      copyButton.addEventListener("click", () => this.handleCopyPrivateKey());
    }

    // Logout
    const logoutContainer = document.getElementById("logoutContainer");
    if (logoutContainer) {
      logoutContainer.addEventListener("click", () => this.handleLogout());
    }

    // Tab switching
    document.querySelectorAll(".tabs li").forEach((tab) => {
      tab.addEventListener("click", () => {
        const isLogin = tab.closest("#loginModal");
        if (isLogin) {
          this.switchLoginTab(tab);
        } else {
          this.switchRegisterTab(tab);
        }
      });
    });
  }

  async handlePrivateKeyLogin() {
    const errorElement = document.querySelector("#privateKeyError");
    if (errorElement) errorElement.textContent = "";

    const privateKey = document.getElementById("loginPrivateKey").value;
    if (!privateKey) {
      if (errorElement)
        errorElement.textContent = "Please enter your private key";
      return;
    }

    try {
      await window.nostrClient.initialize(SignerType.PrivateKey, privateKey);
      this.authorizedClient = new AuthorizedClient(
        window.nostrClient,
        this.apiBase,
      );
      await this.performLogin();
    } catch (error) {
      console.error("Private key login failed:", error);
      if (errorElement) {
        if (error.message === "UNAUTHORIZED") {
          errorElement.textContent =
            "No account found. Please click Sign Up to create a new account.";
        } else {
          errorElement.textContent = "Login failed. Please try again.";
        }
      }
    }
  }

  async handleExtensionLogin() {
    const errorElement = document.querySelector("#extensionLoginError");
    if (errorElement) errorElement.textContent = "";

    try {
      await window.nostrClient.initialize(SignerType.NIP07, null);
      this.authorizedClient = new AuthorizedClient(
        window.nostrClient,
        this.apiBase,
      );
      await this.performLogin();
    } catch (error) {
      console.error("Extension login failed:", error);
      if (errorElement) {
        if (error.message === "UNAUTHORIZED") {
          errorElement.textContent =
            "No account found. Please click Sign Up to register with your extension.";
        } else if (error.message.includes("No NIP-07")) {
          errorElement.textContent =
            "No Nostr extension found. Please install nos2x, Alby, or another NIP-07 compatible extension.";
        } else {
          errorElement.textContent = "Login failed. Please try again.";
        }
      }
    }
  }

  async handleRegisterStep1() {
    try {
      await window.nostrClient.initialize(SignerType.PrivateKey, null);
      this.authorizedClient = new AuthorizedClient(
        window.nostrClient,
        this.apiBase,
      );

      const privateKeyDisplay = document.getElementById("privateKeyDisplay");
      if (privateKeyDisplay) {
        privateKeyDisplay.value = await window.nostrClient.getPrivateKey();
      }

      const step1 = document.getElementById("registerStep1");
      const step2 = document.getElementById("registerStep2");
      if (step1) step1.classList.remove("is-hidden");
      if (step2) step2.classList.add("is-hidden");

      const button = document.getElementById("registerStep1Button");
      if (button) button.disabled = true;
    } catch (error) {
      console.error("Failed to generate private key:", error);
    }
  }

  async handleRegistrationComplete() {
    try {
      await this.performRegistration();
      await this.performLogin();
      this.showRegistrationSuccess();
    } catch (error) {
      console.error("Registration failed:", error);
    }
  }

  async handleExtensionRegistration() {
    const errorElement = document.querySelector("#extensionRegisterError");
    if (errorElement) errorElement.textContent = "";

    try {
      await window.nostrClient.initialize(SignerType.NIP07, null);
      this.authorizedClient = new AuthorizedClient(
        window.nostrClient,
        this.apiBase,
      );
      await this.performRegistration();
      await this.performLogin();
    } catch (error) {
      console.error("Extension registration failed:", error);
      if (errorElement) {
        if (error.message.includes("No NIP-07")) {
          errorElement.textContent =
            "No Nostr extension found. Please install nos2x, Alby, or another NIP-07 compatible extension.";
        } else {
          errorElement.textContent =
            "Registration failed. If you have already registered, please go to Login. Otherwise, try registering again.";
        }
      }
    }
  }

  handleCopyPrivateKey() {
    const privateKey = document.getElementById("privateKeyDisplay").value;
    navigator.clipboard.writeText(privateKey);
  }

  async performRegistration() {
    const pubkey = await window.nostrClient.getPublicKey();
    const wallet = await new TaprootWalletBuilder()
      .network(this.network)
      .nostr_client(window.nostrClient)
      .build();

    const payload = await wallet.getEncryptedMasterKey(pubkey);
    const response = await this.authorizedClient.post(
      `${this.apiBase}/api/v1/users/register`,
      payload,
    );

    if (!response.ok) {
      throw new Error("Registration failed");
    }
  }

  async performLogin() {
    const response = await this.authorizedClient.post(
      `${this.apiBase}/api/v1/users/login`,
    );

    if (response.status === 401 || response.status === 403) {
      throw new Error("UNAUTHORIZED");
    }

    if (!response.ok) {
      throw new Error("Login failed");
    }

    const { encrypted_bitcoin_private_key, network } = await response.json();
    if (this.network !== network) {
      throw new Error(
        `Invalid network, coordinator ${this.network} doesn't match wallet ${network}`,
      );
    }

    const wallet = await new TaprootWalletBuilder()
      .network(network)
      .nostr_client(window.nostrClient)
      .encrypted_key(encrypted_bitcoin_private_key)
      .build();

    window.taprootWallet = wallet;
    this.onLoginSuccess();
  }

  showRegistrationSuccess() {
    const step1 = document.getElementById("registerStep1");
    const step2 = document.getElementById("registerStep2");
    if (step1) step1.classList.add("is-hidden");
    if (step2) step2.classList.remove("is-hidden");

    setTimeout(() => {
      const registerModal = document.querySelector("#registerModal");
      if (registerModal) {
        registerModal.classList.remove("is-active");
        document.documentElement.classList.remove("is-clipped");
      }
    }, 2000);
  }

  handleLogout() {
    window.taprootWallet = null;
    window.nostrClient = new NostrClientWrapper();

    const authButtons = document.getElementById("authButtons");
    const logoutContainer = document.getElementById("logoutContainer");
    if (authButtons) authButtons.classList.remove("is-hidden");
    if (logoutContainer) logoutContainer.classList.add("is-hidden");

    // Clear sensitive data
    const loginInput = document.getElementById("loginPrivateKey");
    const displayInput = document.getElementById("privateKeyDisplay");
    if (loginInput) loginInput.value = "";
    if (displayInput) displayInput.value = "";

    // Navigate to competitions
    const competitionsLink = document.querySelector('[hx-get="/competitions"]');
    if (competitionsLink) competitionsLink.click();
  }

  onLoginSuccess() {
    const authButtons = document.getElementById("authButtons");
    const logoutContainer = document.getElementById("logoutContainer");
    if (authButtons) authButtons.classList.add("is-hidden");
    if (logoutContainer) logoutContainer.classList.remove("is-hidden");

    // Close all modals
    if (window.closeAllModals) window.closeAllModals();
  }

  switchLoginTab(tab) {
    document
      .querySelectorAll("#loginModal .tabs li")
      .forEach((t) => t.classList.remove("is-active"));
    tab.classList.add("is-active");

    const target = tab.dataset.target;
    const privateKeyLogin = document.getElementById("privateKeyLogin");
    const extensionLogin = document.getElementById("extensionLogin");
    if (privateKeyLogin)
      privateKeyLogin.classList.toggle(
        "is-hidden",
        target !== "privateKeyLogin",
      );
    if (extensionLogin)
      extensionLogin.classList.toggle("is-hidden", target !== "extensionLogin");
  }

  switchRegisterTab(tab) {
    document
      .querySelectorAll("#registerModal .tabs li")
      .forEach((t) => t.classList.remove("is-active"));
    tab.classList.add("is-active");

    const target = tab.dataset.target;
    const registerPrivateKey = document.getElementById("registerPrivateKey");
    const registerExtension = document.getElementById("registerExtension");
    if (registerPrivateKey)
      registerPrivateKey.classList.toggle(
        "is-hidden",
        target !== "registerPrivateKey",
      );
    if (registerExtension)
      registerExtension.classList.toggle(
        "is-hidden",
        target !== "registerExtension",
      );
  }
}

// Export for use by other modules
export { requiresAuth, isLoggedIn, generateAuthHeader, AuthManager };
