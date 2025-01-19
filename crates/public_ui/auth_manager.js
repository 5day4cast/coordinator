import init, {
  TaprootWallet,
  TaprootWalletBuilder,
  NostrClientWrapper,
  SignerType,
} from "./dist/client_validator.js";

import { AuthorizedClient } from "./authorized_client.js";

export async function setupAuthManager(apiBase, network) {
  const auth_manager = new AuthManager(apiBase, network);
  console.log("initialized auth manager");
  return auth_manager;
}

class AuthManager {
  constructor(apiBase, network) {
    this.apiBase = apiBase;
    this.network = network;
    this.initializeComponents();
    this.attachEventListeners();
  }

  async initializeComponents() {
    await init();
    window.nostrClient = new NostrClientWrapper();
  }

  attachEventListeners() {
    // Auth related buttons
    document
      .getElementById("logoutNavClick")
      .addEventListener("click", () => this.handleLogout());

    document
      .getElementById("loginButton")
      .addEventListener("click", () => this.handlePrivateKeyLogin());

    document
      .getElementById("extensionLoginButton")
      .addEventListener("click", () => this.handleExtensionLogin());

    // Registration related buttons
    document
      .getElementById("registerNavClick")
      .addEventListener("click", () => this.handleRegisterStep1());

    document
      .getElementById("registerStep1Button")
      .addEventListener("click", () => this.handleRegistrationComplete());

    document
      .getElementById("extensionRegisterButton")
      .addEventListener("click", () => this.handleExtensionRegistration());

    document
      .getElementById("privateKeySavedCheckbox")
      .addEventListener("change", (e) => {
        console.log("private key saved", e);
        document.getElementById("registerStep1Button").disabled =
          !e.target.checked;
      });

    document
      .getElementById("copyPrivateKey")
      .addEventListener("click", () => this.handleCopyPrivateKey());

    // Tab switching for both modals
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
    if (errorElement) {
      errorElement.textContent = "";
    }

    const privateKey = document.getElementById("loginPrivateKey").value;
    if (!privateKey) {
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
    if (errorElement) {
      errorElement.textContent = "";
    }

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
      privateKeyDisplay.value = await window.nostrClient.getPrivateKey();

      // Show step 1 (in case it was hidden)
      document.getElementById("registerStep1").classList.remove("is-hidden");
      document.getElementById("registerStep2").classList.add("is-hidden");

      document.getElementById("registerStep1Button").disabled = true;
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
    if (errorElement) {
      errorElement.textContent = "";
    }

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
    console.log(privateKey);
    navigator.clipboard.writeText(privateKey);
  }

  async performRegistration() {
    const pubkey = await window.nostrClient.getPublicKey();

    const wallet = await new TaprootWalletBuilder()
      .network(this.network)
      .nostr_client(window.nostrClient)
      .build();
    console.log("pubkey", pubkey);
    const payload = await wallet.getEncryptedMasterKey(pubkey);
    console.log("payload", payload);

    const response = await this.authorizedClient.post(
      `${this.apiBase}/users/register`,
      payload,
    );

    if (!response.ok) {
      throw new Error("Registration failed");
    }
  }

  async performLogin() {
    const response = await this.authorizedClient.post(
      `${this.apiBase}/users/login`,
    );

    if (response.status === 401 || response.status === 403) {
      throw new Error("UNAUTHORIZED");
    }

    if (!response.ok) {
      throw new Error("Login failed");
    }

    const { encrypted_bitcoin_private_key, network } = await response.json();
    if (this.network != network) {
      throw new Error(
        `Invalid network, cooridinator ${this.network} doesn't match wallet  ${network}`,
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
    document.getElementById("registerStep1").classList.add("is-hidden");
    document.getElementById("registerStep2").classList.remove("is-hidden");

    // After 2 seconds, close the modal
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

    document.getElementById("authButtons").classList.remove("is-hidden");
    document.getElementById("logoutNavClick").classList.add("is-hidden");

    // Clear any sensitive data
    document.getElementById("loginPrivateKey").value = "";
    document.getElementById("privateKeyDisplay").value = "";
  }

  onLoginSuccess() {
    document.getElementById("authButtons").classList.add("is-hidden");
    document.getElementById("logoutNavClick").classList.remove("is-hidden");

    // Close any open modals
    document.querySelectorAll(".modal.is-active").forEach((modal) => {
      modal.classList.remove("is-active");
    });
    document.documentElement.classList.remove("is-clipped");

    const competitionsNavClick = document.getElementById(
      "allCompetitionsNavClick",
    );
    competitionsNavClick.click();
  }

  switchLoginTab(tab) {
    document
      .querySelectorAll("#loginModal .tabs li")
      .forEach((t) => t.classList.remove("is-active"));
    tab.classList.add("is-active");

    const target = tab.dataset.target;
    document
      .getElementById("privateKeyLogin")
      .classList.toggle("is-hidden", target !== "privateKeyLogin");
    document
      .getElementById("extensionLogin")
      .classList.toggle("is-hidden", target !== "extensionLogin");
  }

  switchRegisterTab(tab) {
    document
      .querySelectorAll("#registerModal .tabs li")
      .forEach((t) => t.classList.remove("is-active"));
    tab.classList.add("is-active");

    const target = tab.dataset.target;
    document
      .getElementById("registerPrivateKey")
      .classList.toggle("is-hidden", target !== "registerPrivateKey");
    document
      .getElementById("registerExtension")
      .classList.toggle("is-hidden", target !== "registerExtension");
  }
}
