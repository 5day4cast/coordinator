function resetLoginModal() {
  const emailError = document.querySelector("#emailLoginError");
  if (emailError) emailError.textContent = "";

  const extensionError = document.querySelector("#extensionLoginError");
  if (extensionError) extensionError.textContent = "";

  const emailInput = document.getElementById("loginEmail");
  if (emailInput) emailInput.value = "";

  const passwordInput = document.getElementById("loginPassword");
  if (passwordInput) passwordInput.value = "";

  document
    .querySelector("#loginModal .tabs li[data-target='emailLogin']")
    ?.click();
}

function resetRegisterModal() {
  const emailError = document.querySelector("#emailRegisterError");
  if (emailError) emailError.textContent = "";

  const extensionError = document.querySelector("#extensionRegisterError");
  if (extensionError) extensionError.textContent = "";

  const emailInput = document.getElementById("registerEmailInput");
  if (emailInput) emailInput.value = "";

  const passwordInput = document.getElementById("registerPassword");
  if (passwordInput) passwordInput.value = "";

  const confirmInput = document.getElementById("registerPasswordConfirm");
  if (confirmInput) confirmInput.value = "";

  const display = document.getElementById("emailNsecDisplay");
  if (display) display.value = "";

  const checkbox = document.getElementById("emailNsecSavedCheckbox");
  if (checkbox) checkbox.checked = false;

  const step2Button = document.getElementById("emailRegisterStep2Button");
  if (step2Button) step2Button.disabled = true;

  document.getElementById("emailRegisterStep1")?.classList.remove("is-hidden");
  document.getElementById("emailRegisterStep2")?.classList.add("is-hidden");
  document.getElementById("emailRegisterStep3")?.classList.add("is-hidden");

  document
    .querySelector("#registerModal .tabs li[data-target='registerEmail']")
    ?.click();
}

function resetForgotPasswordModal() {
  const error = document.querySelector("#forgotPasswordError");
  if (error) error.textContent = "";

  const emailInput = document.getElementById("forgotEmail");
  if (emailInput) emailInput.value = "";

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
    // Email login
    document
      .getElementById("emailLoginButton")
      ?.addEventListener("click", () => this.handleEmailLogin());

    // Extension login
    document
      .getElementById("extensionLoginButton")
      ?.addEventListener("click", () => this.handleExtensionLogin());

    // Email registration steps
    document
      .getElementById("emailRegisterStep1Button")
      ?.addEventListener("click", () => this.handleEmailRegisterStep1());
    document
      .getElementById("emailRegisterStep2Button")
      ?.addEventListener("click", () => this.handleEmailRegisterStep2());

    // Extension registration
    document
      .getElementById("extensionRegisterButton")
      ?.addEventListener("click", () => this.handleExtensionRegistration());

    // Copy nsec button
    document
      .getElementById("copyEmailNsec")
      ?.addEventListener("click", () => this.handleCopyNsec());

    // Logout
    document
      .getElementById("logoutContainer")
      ?.addEventListener("click", () => this.handleLogout());

    // nsec saved checkbox enables step 2 button
    document
      .getElementById("emailNsecSavedCheckbox")
      ?.addEventListener("change", (e) => {
        const btn = document.getElementById("emailRegisterStep2Button");
        if (btn) btn.disabled = !e.target.checked;
      });

    // Forgot password steps
    document
      .getElementById("forgotStep1Button")
      ?.addEventListener("click", () => this.handleForgotStep1());
    document
      .getElementById("forgotStep2Button")
      ?.addEventListener("click", () => this.handleForgotStep2());
    document
      .getElementById("forgotStep3Button")
      ?.addEventListener("click", () => this.handleForgotStep3());

    // Tab switching
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

  async handleEmailLogin() {
    const errorElement = document.querySelector("#emailLoginError");
    if (errorElement) errorElement.textContent = "";

    const email = document.getElementById("loginEmail")?.value?.trim();
    const password = document.getElementById("loginPassword")?.value;

    if (!email || !password) {
      if (errorElement)
        errorElement.textContent = "Please enter email and password";
      return;
    }

    try {
      // Call login endpoint to get encrypted nsec
      const response = await fetch(`${this.apiBase}/api/v1/users/email/login`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ email, password }),
      });

      if (response.status === 401) {
        if (errorElement)
          errorElement.textContent = "Invalid email or password";
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

      // Decrypt nsec with password (client-side WASM)
      const nsec = await window.decryptNsecWithPassword(
        encrypted_nsec,
        password,
      );

      // Initialize NostrClient with decrypted nsec
      await window.nostrClient.initialize(window.SignerType.PrivateKey, nsec);
      this.authorizedClient = new window.AuthorizedClient(
        window.nostrClient,
        this.apiBase,
      );

      // Build taproot wallet
      window.taprootWallet = await new window.TaprootWalletBuilder()
        .network(network)
        .nostr_client(window.nostrClient)
        .encrypted_key(encrypted_bitcoin_private_key)
        .build();

      this.onLoginSuccess();
    } catch (error) {
      console.error("Email login failed:", error);
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

  async handleEmailRegisterStep1() {
    const errorElement = document.querySelector("#emailRegisterError");
    if (errorElement) errorElement.textContent = "";

    const email = document.getElementById("registerEmailInput")?.value?.trim();
    const password = document.getElementById("registerPassword")?.value;
    const confirmPassword = document.getElementById(
      "registerPasswordConfirm",
    )?.value;

    if (!email || !password || !confirmPassword) {
      if (errorElement) errorElement.textContent = "Please fill in all fields";
      return;
    }

    if (password !== confirmPassword) {
      if (errorElement) errorElement.textContent = "Passwords do not match";
      return;
    }

    if (password.length < 8) {
      if (errorElement)
        errorElement.textContent = "Password must be at least 8 characters";
      return;
    }

    try {
      // Generate new nsec (client-side WASM)
      await window.nostrClient.initialize(window.SignerType.PrivateKey, null);
      const nsec = await window.nostrClient.getPrivateKey();

      // Encrypt nsec with password (client-side WASM)
      const encryptedNsec = await window.encryptNsecWithPassword(
        nsec,
        password,
      );

      // Store for step 2
      this.pendingNsec = nsec;
      this.pendingEncryptedNsec = encryptedNsec;
      this.pendingEmail = email;
      this.pendingPassword = password;

      // Show nsec to user
      const display = document.getElementById("emailNsecDisplay");
      if (display) display.value = nsec;

      // Move to step 2
      document.getElementById("emailRegisterStep1")?.classList.add("is-hidden");
      document
        .getElementById("emailRegisterStep2")
        ?.classList.remove("is-hidden");
    } catch (error) {
      console.error("Registration step 1 failed:", error);
      if (errorElement)
        errorElement.textContent = "Failed to generate keys. Please try again.";
    }
  }

  async handleEmailRegisterStep2() {
    const errorElement = document.querySelector("#emailRegisterError");
    if (errorElement) errorElement.textContent = "";

    if (
      !this.pendingNsec ||
      !this.pendingEncryptedNsec ||
      !this.pendingEmail ||
      !this.pendingPassword
    ) {
      if (errorElement)
        errorElement.textContent = "Registration state lost. Please try again.";
      resetRegisterModal();
      return;
    }

    try {
      // Create authorized client for registration
      this.authorizedClient = new window.AuthorizedClient(
        window.nostrClient,
        this.apiBase,
      );

      // Create wallet and get encrypted bitcoin key
      const pubkey = await window.nostrClient.getPublicKey();
      const wallet = await new window.TaprootWalletBuilder()
        .network(this.network)
        .nostr_client(window.nostrClient)
        .build();

      const { encrypted_bitcoin_private_key } =
        await wallet.getEncryptedMasterKey(pubkey);

      // Register with server
      const response = await fetch(
        `${this.apiBase}/api/v1/users/email/register`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            email: this.pendingEmail,
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
        if (
          response.status === 400 &&
          data.error?.includes("already registered")
        ) {
          if (errorElement)
            errorElement.textContent =
              "Email already registered. Please log in.";
        } else {
          if (errorElement)
            errorElement.textContent =
              data.error || "Registration failed. Please try again.";
        }
        return;
      }

      // Set up taproot wallet
      window.taprootWallet = wallet;

      // Clear pending state
      this.pendingNsec = null;
      this.pendingEncryptedNsec = null;
      this.pendingEmail = null;
      this.pendingPassword = null;

      // Show success step
      document.getElementById("emailRegisterStep2")?.classList.add("is-hidden");
      document
        .getElementById("emailRegisterStep3")
        ?.classList.remove("is-hidden");

      // Auto-close after delay
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
    const nsec = document.getElementById("emailNsecDisplay")?.value;
    if (nsec) {
      navigator.clipboard.writeText(nsec);
    }
  }

  async handleForgotStep1() {
    const errorElement = document.querySelector("#forgotPasswordError");
    if (errorElement) errorElement.textContent = "";

    const email = document.getElementById("forgotEmail")?.value?.trim();
    if (!email) {
      if (errorElement) errorElement.textContent = "Please enter your email";
      return;
    }

    try {
      const response = await fetch(
        `${this.apiBase}/api/v1/users/email/forgot-password`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ email }),
        },
      );

      if (response.status === 404) {
        if (errorElement)
          errorElement.textContent = "No account found with that email";
        return;
      }

      if (!response.ok) {
        throw new Error("Failed to initiate password reset");
      }

      const { challenge, nostr_pubkey } = await response.json();
      this.forgotChallenge = challenge;
      this.forgotNpub = nostr_pubkey;
      this.forgotEmail = email;

      // Move to step 2
      document.getElementById("forgotStep1")?.classList.add("is-hidden");
      document.getElementById("forgotStep2")?.classList.remove("is-hidden");
    } catch (error) {
      console.error("Forgot password step 1 failed:", error);
      if (errorElement)
        errorElement.textContent = "Request failed. Please try again.";
    }
  }

  async handleForgotStep2() {
    const errorElement = document.querySelector("#forgotPasswordError");
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
      // Verify nsec matches the account's npub
      await window.nostrClient.initialize(window.SignerType.PrivateKey, nsec);
      const derivedNpub = await window.nostrClient.getPublicKey();

      if (derivedNpub !== this.forgotNpub) {
        if (errorElement)
          errorElement.textContent =
            "This recovery key does not match the account";
        return;
      }

      // Sign challenge to prove ownership
      this.forgotSignedChallenge = await window.signForgotPasswordChallenge(
        nsec,
        this.forgotChallenge,
      );
      this.forgotNsec = nsec;

      // Move to step 3
      document.getElementById("forgotStep2")?.classList.add("is-hidden");
      document.getElementById("forgotStep3")?.classList.remove("is-hidden");
    } catch (error) {
      console.error("Forgot password step 2 failed:", error);
      if (errorElement)
        errorElement.textContent = "Invalid recovery key. Please try again.";
    }
  }

  async handleForgotStep3() {
    const errorElement = document.querySelector("#forgotPasswordError");
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

    if (newPassword.length < 8) {
      if (errorElement)
        errorElement.textContent = "Password must be at least 8 characters";
      return;
    }

    if (!this.forgotSignedChallenge || !this.forgotNsec || !this.forgotEmail) {
      if (errorElement)
        errorElement.textContent = "Session expired. Please start over.";
      resetForgotPasswordModal();
      return;
    }

    try {
      // Re-encrypt nsec with new password
      const newEncryptedNsec = await window.encryptNsecWithPassword(
        this.forgotNsec,
        newPassword,
      );

      // Send reset request
      const response = await fetch(
        `${this.apiBase}/api/v1/users/email/reset-password`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            email: this.forgotEmail,
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

      // Clear forgot state
      this.forgotChallenge = null;
      this.forgotNpub = null;
      this.forgotEmail = null;
      this.forgotSignedChallenge = null;
      this.forgotNsec = null;

      // Close modal and show login
      window.closeModal(document.getElementById("forgotPasswordModal"));
      resetLoginModal();
      window.openModal(document.getElementById("loginModal"));

      // Show success message in login modal
      const loginError = document.querySelector("#emailLoginError");
      if (loginError) {
        loginError.textContent = "Password reset successful. Please log in.";
        loginError.classList.remove("has-text-danger");
        loginError.classList.add("has-text-success");
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

    // Clear any sensitive inputs
    document.getElementById("loginEmail")?.value &&
      (document.getElementById("loginEmail").value = "");
    document.getElementById("loginPassword")?.value &&
      (document.getElementById("loginPassword").value = "");

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
      .getElementById("emailLogin")
      ?.classList.toggle("is-hidden", target !== "emailLogin");
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
      .getElementById("registerEmail")
      ?.classList.toggle("is-hidden", target !== "registerEmail");
    document
      .getElementById("registerExtension")
      ?.classList.toggle("is-hidden", target !== "registerExtension");
  }
}

window.AuthManager = AuthManager;
