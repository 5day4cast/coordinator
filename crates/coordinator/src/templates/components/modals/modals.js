function resetLoginModal() {
  const usernameError = document.querySelector("#usernameLoginError");
  if (usernameError) usernameError.textContent = "";

  const extensionError = document.querySelector("#extensionLoginError");
  if (extensionError) extensionError.textContent = "";

  const usernameInput = document.getElementById("loginUsername");
  if (usernameInput) usernameInput.value = "";

  const passwordInput = document.getElementById("loginPassword");
  if (passwordInput) passwordInput.value = "";

  document
    .querySelector("#loginModal .tabs li[data-target='usernameLogin']")
    ?.click();
}

function resetRegisterModal() {
  const usernameError = document.querySelector("#usernameRegisterError");
  if (usernameError) usernameError.textContent = "";

  const extensionError = document.querySelector("#extensionRegisterError");
  if (extensionError) extensionError.textContent = "";

  const usernameInput = document.getElementById("registerUsernameInput");
  if (usernameInput) usernameInput.value = "";

  const passwordInput = document.getElementById("registerPassword");
  if (passwordInput) passwordInput.value = "";

  const confirmInput = document.getElementById("registerPasswordConfirm");
  if (confirmInput) confirmInput.value = "";

  const display = document.getElementById("usernameNsecDisplay");
  if (display) display.value = "";

  const checkbox = document.getElementById("usernameNsecSavedCheckbox");
  if (checkbox) checkbox.checked = false;

  const step2Button = document.getElementById("usernameRegisterStep2Button");
  if (step2Button) step2Button.disabled = true;

  document
    .getElementById("usernameRegisterStep1")
    ?.classList.remove("is-hidden");
  document.getElementById("usernameRegisterStep2")?.classList.add("is-hidden");
  document.getElementById("usernameRegisterStep3")?.classList.add("is-hidden");

  document
    .querySelector("#registerModal .tabs li[data-target='registerUsername']")
    ?.click();
}

function resetForgotPasswordModal() {
  const error = document.querySelector("#forgotStep1Error");
  if (error) error.textContent = "";

  const usernameInput = document.getElementById("forgotUsername");
  if (usernameInput) usernameInput.value = "";

  const nsecInput = document.getElementById("forgotNsec");
  if (nsecInput) nsecInput.value = "";

  const newPasswordInput = document.getElementById("forgotNewPassword");
  if (newPasswordInput) newPasswordInput.value = "";

  const confirmInput = document.getElementById("forgotNewPasswordConfirm");
  if (confirmInput) confirmInput.value = "";

  document.getElementById("forgotStep1")?.classList.remove("is-hidden");
  document.getElementById("forgotStep2")?.classList.add("is-hidden");
  document.getElementById("forgotStep3")?.classList.add("is-hidden");
}

function setupAuthModals(authManager) {
  document.getElementById("loginNavClick")?.addEventListener("click", () => {
    resetLoginModal();
    window.openModal(document.getElementById("loginModal"));
  });

  document.getElementById("registerNavClick")?.addEventListener("click", () => {
    resetRegisterModal();
    window.openModal(document.getElementById("registerModal"));
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
    .getElementById("closeForgotPasswordModal")
    ?.addEventListener("click", () => {
      resetForgotPasswordModal();
      window.closeModal(document.getElementById("forgotPasswordModal"));
    });

  document
    .getElementById("showRegisterButton")
    ?.addEventListener("click", () => {
      window.closeModal(document.getElementById("loginModal"));
      resetRegisterModal();
      window.openModal(document.getElementById("registerModal"));
    });

  document.getElementById("goToLoginButton")?.addEventListener("click", () => {
    window.closeModal(document.getElementById("registerModal"));
    window.openModal(document.getElementById("loginModal"));
  });

  document
    .getElementById("forgotPasswordLink")
    ?.addEventListener("click", (e) => {
      e.preventDefault();
      window.closeModal(document.getElementById("loginModal"));
      resetForgotPasswordModal();
      window.openModal(document.getElementById("forgotPasswordModal"));
    });

  document
    .getElementById("backToLoginFromForgot")
    ?.addEventListener("click", (e) => {
      e.preventDefault();
      window.closeModal(document.getElementById("forgotPasswordModal"));
      resetLoginModal();
      window.openModal(document.getElementById("loginModal"));
    });
}

window.resetLoginModal = resetLoginModal;
window.resetRegisterModal = resetRegisterModal;
window.resetForgotPasswordModal = resetForgotPasswordModal;
window.setupAuthModals = setupAuthModals;

class AuthManager {
  constructor(apiBase, network) {
    this.apiBase = apiBase;
    this.network = network;
    this.authorizedClient = null;
    this.pendingNsec = null;
    this.pendingEncryptedNsec = null;
    this.forgotChallenge = null;
    this.forgotNpub = null;
  }

  attachEventListeners() {
    document
      .getElementById("usernameLoginButton")
      ?.addEventListener("click", () => this.handleUsernameLogin());

    document
      .getElementById("extensionLoginButton")
      ?.addEventListener("click", () => this.handleExtensionLogin());

    document
      .getElementById("usernameRegisterStep1Button")
      ?.addEventListener("click", () => this.handleUsernameRegisterStep1());
    document
      .getElementById("usernameRegisterStep2Button")
      ?.addEventListener("click", () => this.handleUsernameRegisterStep2());

    document
      .getElementById("extensionRegisterButton")
      ?.addEventListener("click", () => this.handleExtensionRegistration());

    document
      .getElementById("copyUsernameNsec")
      ?.addEventListener("click", () => this.handleCopyNsec());

    document
      .getElementById("logoutContainer")
      ?.addEventListener("click", () => this.handleLogout());

    document
      .getElementById("usernameNsecSavedCheckbox")
      ?.addEventListener("change", (e) => {
        const btn = document.getElementById("usernameRegisterStep2Button");
        if (btn) btn.disabled = !e.target.checked;
      });

    document
      .getElementById("forgotStep1Button")
      ?.addEventListener("click", () => this.handleForgotStep1());
    document
      .getElementById("forgotStep2Button")
      ?.addEventListener("click", () => this.handleForgotStep2());
    document
      .getElementById("forgotStep3Button")
      ?.addEventListener("click", () => this.handleForgotStep3());

    document.querySelectorAll(".tabs li").forEach((tab) => {
      tab.addEventListener("click", () => {
        const modal = tab.closest(".modal");
        if (modal?.id === "loginModal") {
          this.switchLoginTab(tab);
        } else if (modal?.id === "registerModal") {
          this.switchRegisterTab(tab);
        }
      });
    });
  }

  async handleUsernameLogin() {
    const errorElement = document.querySelector("#usernameLoginError");
    if (errorElement) errorElement.textContent = "";

    const username = document.getElementById("loginUsername")?.value?.trim();
    const password = document.getElementById("loginPassword")?.value;

    if (!username || !password) {
      if (errorElement)
        errorElement.textContent = "Please enter username and password";
      return;
    }

    try {
      const response = await fetch(
        `${this.apiBase}/api/v1/users/username/login`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ username, password }),
        },
      );

      if (response.status === 401) {
        if (errorElement)
          errorElement.textContent = "Invalid username or password";
        return;
      }

      if (!response.ok) {
        throw new Error("Login failed");
      }

      const { encrypted_nsec, encrypted_bitcoin_private_key, network } =
        await response.json();

      if (this.network !== network) {
        throw new Error(
          `Network mismatch: coordinator ${this.network}, wallet ${network}`,
        );
      }

      const nsec = await window.decryptNsecWithPassword(
        encrypted_nsec,
        password,
      );

      await window.nostrClient.initialize(window.SignerType.PrivateKey, nsec);
      this.authorizedClient = new window.AuthorizedClient(
        window.nostrClient,
        this.apiBase,
      );

      window.taprootWallet = await new window.TaprootWalletBuilder()
        .network(network)
        .nostr_client(window.nostrClient)
        .encrypted_key(encrypted_bitcoin_private_key)
        .build();

      this.onLoginSuccess();
    } catch (error) {
      console.error("Username login failed:", error);
      if (errorElement) {
        errorElement.textContent =
          error.message.includes("decrypt") ||
          error.message.includes("password")
            ? "Invalid password"
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

  async handleUsernameRegisterStep1() {
    const errorElement = document.querySelector("#usernameRegisterError");
    if (errorElement) errorElement.textContent = "";

    const username = document
      .getElementById("registerUsernameInput")
      ?.value?.trim();
    const password = document.getElementById("registerPassword")?.value;
    const confirmPassword = document.getElementById(
      "registerPasswordConfirm",
    )?.value;

    if (!username || !password || !confirmPassword) {
      if (errorElement) errorElement.textContent = "Please fill in all fields";
      return;
    }

    const usernameError = this.validateUsername(username);
    if (usernameError) {
      if (errorElement) errorElement.textContent = usernameError;
      return;
    }

    if (password !== confirmPassword) {
      if (errorElement) errorElement.textContent = "Passwords do not match";
      return;
    }

    const passwordError = this.validatePasswordStrength(password);
    if (passwordError) {
      if (errorElement) errorElement.textContent = passwordError;
      return;
    }

    try {
      await window.nostrClient.initialize(window.SignerType.PrivateKey, null);
      const nsec = await window.nostrClient.getPrivateKey();

      const encryptedNsec = await window.encryptNsecWithPassword(
        nsec,
        password,
      );

      this.pendingNsec = nsec;
      this.pendingEncryptedNsec = encryptedNsec;
      this.pendingUsername = username;
      this.pendingPassword = password;

      const display = document.getElementById("usernameNsecDisplay");
      if (display) display.value = nsec;

      document
        .getElementById("usernameRegisterStep1")
        ?.classList.add("is-hidden");
      document
        .getElementById("usernameRegisterStep2")
        ?.classList.remove("is-hidden");
    } catch (error) {
      console.error("Registration step 1 failed:", error);
      if (errorElement)
        errorElement.textContent = "Failed to generate keys. Please try again.";
    }
  }

  async handleUsernameRegisterStep2() {
    const errorElement = document.querySelector("#usernameRegisterStep2Error");
    if (errorElement) errorElement.textContent = "";

    if (
      !this.pendingNsec ||
      !this.pendingEncryptedNsec ||
      !this.pendingUsername ||
      !this.pendingPassword
    ) {
      if (errorElement)
        errorElement.textContent = "Registration state lost. Please try again.";
      resetRegisterModal();
      return;
    }

    try {
      this.authorizedClient = new window.AuthorizedClient(
        window.nostrClient,
        this.apiBase,
      );

      const pubkey = await window.nostrClient.getPublicKey();
      const wallet = await new window.TaprootWalletBuilder()
        .network(this.network)
        .nostr_client(window.nostrClient)
        .build();

      const { encrypted_bitcoin_private_key } =
        await wallet.getEncryptedMasterKey(pubkey);

      const response = await fetch(
        `${this.apiBase}/api/v1/users/username/register`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            username: this.pendingUsername,
            password: this.pendingPassword,
            encrypted_nsec: this.pendingEncryptedNsec,
            nostr_pubkey: pubkey,
            encrypted_bitcoin_private_key,
            network: this.network,
          }),
        },
      );

      if (!response.ok) {
        const data = await response.json().catch(() => ({}));
        if (errorElement)
          errorElement.textContent =
            data.error || "Registration failed. Please try again.";
        return;
      }

      window.taprootWallet = wallet;

      this.pendingNsec = null;
      this.pendingEncryptedNsec = null;
      this.pendingUsername = null;
      this.pendingPassword = null;

      document
        .getElementById("usernameRegisterStep2")
        ?.classList.add("is-hidden");
      document
        .getElementById("usernameRegisterStep3")
        ?.classList.remove("is-hidden");

      setTimeout(() => {
        this.onLoginSuccess();
      }, 2000);
    } catch (error) {
      console.error("Registration step 2 failed:", error);
      if (errorElement)
        errorElement.textContent = "Registration failed. Please try again.";
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

  handleCopyNsec() {
    const nsec = document.getElementById("usernameNsecDisplay")?.value;
    if (nsec) {
      navigator.clipboard.writeText(nsec);
    }
  }

  validateUsername(username) {
    if (username.length < 3) {
      return "Username must be at least 3 characters";
    }
    if (username.length > 32) {
      return "Username must be at most 32 characters";
    }
    if (!/^[a-zA-Z][a-zA-Z0-9_-]*$/.test(username)) {
      return "Username must start with a letter and contain only letters, numbers, underscores, and hyphens";
    }
    return null;
  }

  validatePasswordStrength(password) {
    if (password.length < 10) {
      return "Password must be at least 10 characters";
    }
    if (!/[a-z]/.test(password)) {
      return "Password must contain a lowercase letter";
    }
    if (!/[A-Z]/.test(password)) {
      return "Password must contain an uppercase letter";
    }
    if (!/[0-9]/.test(password)) {
      return "Password must contain a number";
    }
    if (!/[^a-zA-Z0-9]/.test(password)) {
      return "Password must contain a special character";
    }
    return null;
  }

  async handleForgotStep1() {
    const errorElement = document.querySelector("#forgotStep1Error");
    if (errorElement) errorElement.textContent = "";

    const username = document.getElementById("forgotUsername")?.value?.trim();
    if (!username) {
      if (errorElement) errorElement.textContent = "Please enter your username";
      return;
    }

    try {
      const response = await fetch(
        `${this.apiBase}/api/v1/users/username/forgot-password`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ username }),
        },
      );

      if (!response.ok) {
        throw new Error("Failed to initiate password reset");
      }

      const { challenge, nostr_pubkey } = await response.json();
      this.forgotChallenge = challenge;
      this.forgotNpub = nostr_pubkey;
      this.forgotUsername = username;

      document.getElementById("forgotStep1")?.classList.add("is-hidden");
      document.getElementById("forgotStep2")?.classList.remove("is-hidden");
    } catch (error) {
      console.error("Forgot password step 1 failed:", error);
      if (errorElement)
        errorElement.textContent = "Request failed. Please try again.";
    }
  }

  async handleForgotStep2() {
    const errorElement = document.querySelector("#forgotStep2Error");
    if (errorElement) errorElement.textContent = "";

    const nsec = document.getElementById("forgotNsec")?.value?.trim();
    if (!nsec) {
      if (errorElement)
        errorElement.textContent = "Please enter your recovery key (nsec)";
      return;
    }

    if (!this.forgotChallenge) {
      if (errorElement)
        errorElement.textContent = "Challenge expired. Please start over.";
      resetForgotPasswordModal();
      return;
    }

    try {
      await window.nostrClient.initialize(window.SignerType.PrivateKey, nsec);
      const derivedNpub = await window.nostrClient.getPublicKey();

      if (derivedNpub !== this.forgotNpub) {
        if (errorElement)
          errorElement.textContent =
            "This recovery key does not match the account";
        return;
      }

      this.forgotSignedChallenge = await window.signForgotPasswordChallenge(
        nsec,
        this.forgotChallenge,
      );
      this.forgotNsec = nsec;

      document.getElementById("forgotStep2")?.classList.add("is-hidden");
      document.getElementById("forgotStep3")?.classList.remove("is-hidden");
    } catch (error) {
      console.error("Forgot password step 2 failed:", error);
      if (errorElement)
        errorElement.textContent = "Invalid recovery key. Please try again.";
    }
  }

  async handleForgotStep3() {
    const errorElement = document.querySelector("#forgotStep3Error");
    if (errorElement) errorElement.textContent = "";

    const newPassword = document.getElementById("forgotNewPassword")?.value;
    const confirmPassword = document.getElementById(
      "forgotNewPasswordConfirm",
    )?.value;

    if (!newPassword || !confirmPassword) {
      if (errorElement) errorElement.textContent = "Please fill in all fields";
      return;
    }

    if (newPassword !== confirmPassword) {
      if (errorElement) errorElement.textContent = "Passwords do not match";
      return;
    }

    const passwordError = this.validatePasswordStrength(newPassword);
    if (passwordError) {
      if (errorElement) errorElement.textContent = passwordError;
      return;
    }

    if (
      !this.forgotSignedChallenge ||
      !this.forgotNsec ||
      !this.forgotUsername
    ) {
      if (errorElement)
        errorElement.textContent = "Session expired. Please start over.";
      resetForgotPasswordModal();
      return;
    }

    try {
      const newEncryptedNsec = await window.encryptNsecWithPassword(
        this.forgotNsec,
        newPassword,
      );

      const response = await fetch(
        `${this.apiBase}/api/v1/users/username/reset-password`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            username: this.forgotUsername,
            challenge: this.forgotChallenge,
            signed_event: this.forgotSignedChallenge,
            new_password: newPassword,
            new_encrypted_nsec: newEncryptedNsec,
          }),
        },
      );

      if (!response.ok) {
        const data = await response.json().catch(() => ({}));
        throw new Error(data.error || "Password reset failed");
      }

      this.forgotChallenge = null;
      this.forgotNpub = null;
      this.forgotUsername = null;
      this.forgotSignedChallenge = null;
      this.forgotNsec = null;

      window.closeModal(document.getElementById("forgotPasswordModal"));
      resetLoginModal();
      window.openModal(document.getElementById("loginModal"));

      const loginError = document.querySelector("#usernameLoginError");
      if (loginError) {
        loginError.textContent = "Password reset successful. Please log in.";
        loginError.classList.remove("is-danger");
        loginError.classList.add("is-success");
      }
    } catch (error) {
      console.error("Forgot password step 3 failed:", error);
      if (errorElement)
        errorElement.textContent =
          error.message || "Password reset failed. Please try again.";
    }
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

  handleLogout() {
    window.taprootWallet = null;
    window.nostrClient = new window.NostrClientWrapper();

    document.getElementById("authButtons")?.classList.remove("is-hidden");
    document.getElementById("logoutContainer")?.classList.add("is-hidden");

    const usernameInput = document.getElementById("loginUsername");
    if (usernameInput) usernameInput.value = "";
    const passwordInput = document.getElementById("loginPassword");
    if (passwordInput) passwordInput.value = "";

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
      .getElementById("usernameLogin")
      ?.classList.toggle("is-hidden", target !== "usernameLogin");
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
      .getElementById("registerUsername")
      ?.classList.toggle("is-hidden", target !== "registerUsername");
    document
      .getElementById("registerExtension")
      ?.classList.toggle("is-hidden", target !== "registerExtension");
  }
}

window.AuthManager = AuthManager;
