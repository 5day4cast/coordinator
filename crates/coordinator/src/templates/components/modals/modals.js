function resetLoginModal() {
  const privateKeyError = document.querySelector("#privateKeyError");
  if (privateKeyError) privateKeyError.textContent = "";

  const extensionError = document.querySelector("#extensionLoginError");
  if (extensionError) extensionError.textContent = "";

  const loginInput = document.getElementById("loginPrivateKey");
  if (loginInput) loginInput.value = "";

  document
    .querySelector("#loginModal .tabs li[data-target='privateKeyLogin']")
    ?.click();
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

  document.getElementById("registerStep1")?.classList.remove("is-hidden");
  document.getElementById("registerStep2")?.classList.add("is-hidden");

  document
    .querySelector("#registerModal .tabs li[data-target='registerPrivateKey']")
    ?.click();
}

function setupAuthModals(authManager) {
  document.getElementById("loginNavClick")?.addEventListener("click", () => {
    resetLoginModal();
    window.openModal(document.getElementById("loginModal"));
  });

  document.getElementById("registerNavClick")?.addEventListener("click", () => {
    resetRegisterModal();
    window.openModal(document.getElementById("registerModal"));
    authManager?.handleRegisterStep1();
  });

  document.getElementById("closeLoginModal")?.addEventListener("click", () => {
    window.closeModal(document.getElementById("loginModal"));
  });

  document
    .getElementById("closeResisterModal")
    ?.addEventListener("click", () => {
      resetRegisterModal();
      window.closeModal(document.getElementById("registerModal"));
    });

  document
    .getElementById("showRegisterButton")
    ?.addEventListener("click", () => {
      window.closeModal(document.getElementById("loginModal"));
      resetRegisterModal();
      window.openModal(document.getElementById("registerModal"));
      authManager?.handleRegisterStep1();
    });

  document.getElementById("goToLoginButton")?.addEventListener("click", () => {
    window.closeModal(document.getElementById("registerModal"));
    window.openModal(document.getElementById("loginModal"));
  });
}

window.resetLoginModal = resetLoginModal;
window.resetRegisterModal = resetRegisterModal;
window.setupAuthModals = setupAuthModals;

class AuthManager {
  constructor(apiBase, network) {
    this.apiBase = apiBase;
    this.network = network;
    this.authorizedClient = null;
  }

  attachEventListeners() {
    document
      .getElementById("loginButton")
      ?.addEventListener("click", () => this.handlePrivateKeyLogin());
    document
      .getElementById("extensionLoginButton")
      ?.addEventListener("click", () => this.handleExtensionLogin());
    document
      .getElementById("registerStep1Button")
      ?.addEventListener("click", () => this.handleRegistrationComplete());
    document
      .getElementById("extensionRegisterButton")
      ?.addEventListener("click", () => this.handleExtensionRegistration());
    document
      .getElementById("copyPrivateKey")
      ?.addEventListener("click", () => this.handleCopyPrivateKey());
    document
      .getElementById("logoutContainer")
      ?.addEventListener("click", () => this.handleLogout());

    document
      .getElementById("privateKeySavedCheckbox")
      ?.addEventListener("change", (e) => {
        const btn = document.getElementById("registerStep1Button");
        if (btn) btn.disabled = !e.target.checked;
      });

    document.querySelectorAll(".tabs li").forEach((tab) => {
      tab.addEventListener("click", () => {
        tab.closest("#loginModal")
          ? this.switchLoginTab(tab)
          : this.switchRegisterTab(tab);
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
      await window.nostrClient.initialize(
        window.SignerType.PrivateKey,
        privateKey,
      );
      this.authorizedClient = new window.AuthorizedClient(
        window.nostrClient,
        this.apiBase,
      );
      await this.performLogin();
    } catch (error) {
      console.error("Private key login failed:", error);
      if (errorElement) {
        errorElement.textContent =
          error.message === "UNAUTHORIZED"
            ? "No account found. Please click Sign Up to create a new account."
            : "Login failed. Please try again.";
      }
    }
  }

  async handleExtensionLogin() {
    const errorElement = document.querySelector("#extensionLoginError");
    if (errorElement) errorElement.textContent = "";

    try {
      await window.nostrClient.initialize(window.SignerType.NIP07, null);
      this.authorizedClient = new window.AuthorizedClient(
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
      await window.nostrClient.initialize(window.SignerType.PrivateKey, null);
      this.authorizedClient = new window.AuthorizedClient(
        window.nostrClient,
        this.apiBase,
      );

      const privateKeyDisplay = document.getElementById("privateKeyDisplay");
      if (privateKeyDisplay) {
        privateKeyDisplay.value = await window.nostrClient.getPrivateKey();
      }

      document.getElementById("registerStep1")?.classList.remove("is-hidden");
      document.getElementById("registerStep2")?.classList.add("is-hidden");
      const btn = document.getElementById("registerStep1Button");
      if (btn) btn.disabled = true;
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
      await window.nostrClient.initialize(window.SignerType.NIP07, null);
      this.authorizedClient = new window.AuthorizedClient(
        window.nostrClient,
        this.apiBase,
      );
      await this.performRegistration();
      await this.performLogin();
    } catch (error) {
      console.error("Extension registration failed:", error);
      if (errorElement) {
        errorElement.textContent = error.message.includes("No NIP-07")
          ? "No Nostr extension found. Please install nos2x, Alby, or another NIP-07 compatible extension."
          : "Registration failed. If you have already registered, please go to Login.";
      }
    }
  }

  handleCopyPrivateKey() {
    const privateKey = document.getElementById("privateKeyDisplay").value;
    navigator.clipboard.writeText(privateKey);
  }

  async performRegistration() {
    const pubkey = await window.nostrClient.getPublicKey();
    const wallet = await new window.TaprootWalletBuilder()
      .network(this.network)
      .nostr_client(window.nostrClient)
      .build();

    const payload = await wallet.getEncryptedMasterKey(pubkey);
    const response = await this.authorizedClient.post(
      `${this.apiBase}/api/v1/users/register`,
      payload,
    );

    if (!response.ok) throw new Error("Registration failed");
  }

  async performLogin() {
    const response = await this.authorizedClient.post(
      `${this.apiBase}/api/v1/users/login`,
    );

    if (response.status === 401 || response.status === 403)
      throw new Error("UNAUTHORIZED");
    if (!response.ok) throw new Error("Login failed");

    const { encrypted_bitcoin_private_key, network } = await response.json();
    if (this.network !== network) {
      throw new Error(
        `Invalid network, coordinator ${this.network} doesn't match wallet ${network}`,
      );
    }

    window.taprootWallet = await new window.TaprootWalletBuilder()
      .network(network)
      .nostr_client(window.nostrClient)
      .encrypted_key(encrypted_bitcoin_private_key)
      .build();

    this.onLoginSuccess();
  }

  showRegistrationSuccess() {
    document.getElementById("registerStep1")?.classList.add("is-hidden");
    document.getElementById("registerStep2")?.classList.remove("is-hidden");

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
    window.nostrClient = new window.NostrClientWrapper();

    document.getElementById("authButtons")?.classList.remove("is-hidden");
    document.getElementById("logoutContainer")?.classList.add("is-hidden");

    const loginInput = document.getElementById("loginPrivateKey");
    const displayInput = document.getElementById("privateKeyDisplay");
    if (loginInput) loginInput.value = "";
    if (displayInput) displayInput.value = "";

    document.querySelector('[hx-get="/competitions"]')?.click();
  }

  onLoginSuccess() {
    document.getElementById("authButtons")?.classList.add("is-hidden");
    document.getElementById("logoutContainer")?.classList.remove("is-hidden");
    window.closeAllModals?.();
  }

  switchLoginTab(tab) {
    document
      .querySelectorAll("#loginModal .tabs li")
      .forEach((t) => t.classList.remove("is-active"));
    tab.classList.add("is-active");

    const target = tab.dataset.target;
    document
      .getElementById("privateKeyLogin")
      ?.classList.toggle("is-hidden", target !== "privateKeyLogin");
    document
      .getElementById("extensionLogin")
      ?.classList.toggle("is-hidden", target !== "extensionLogin");
  }

  switchRegisterTab(tab) {
    document
      .querySelectorAll("#registerModal .tabs li")
      .forEach((t) => t.classList.remove("is-active"));
    tab.classList.add("is-active");

    const target = tab.dataset.target;
    document
      .getElementById("registerPrivateKey")
      ?.classList.toggle("is-hidden", target !== "registerPrivateKey");
    document
      .getElementById("registerExtension")
      ?.classList.toggle("is-hidden", target !== "registerExtension");
  }
}

window.AuthManager = AuthManager;
