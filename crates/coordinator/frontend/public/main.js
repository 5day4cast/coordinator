import { displayCompetitions } from "./competitions.js";
import { setupAuthManager } from "./auth_manager.js";
import { SigningProgressUI } from "./signing_progress_ui.js";
import { MusigSessionRegistry } from "./musig_session_registry.js";
import { registerPageReloadHandlers } from "./navbar.js";

const apiBase = API_BASE;
console.log("api location:", apiBase);
const oracleBase = ORACLE_BASE;
console.log("oracle location:", oracleBase);

const network = NETWORK;
console.log("bitcoin network:", network);

let musigRegistry = null;

export function getMusigRegistry() {
  if (!musigRegistry) {
    console.warn("Attempting to access musig registry before initialization");
    return null;
  }
  return musigRegistry;
}

export function cleanupMusigSystem() {
  if (musigRegistry) {
    musigRegistry.clearAllSessions();
    musigRegistry = null;
  }
}

function initializeMusigSystem() {
  if (!musigRegistry) {
    const signingUI = new SigningProgressUI();
    musigRegistry = new MusigSessionRegistry();

    // Connect the components both ways
    signingUI.setRegistry(musigRegistry);
    musigRegistry.addObserver(signingUI);
  }
  return musigRegistry;
}

export async function initializeMusigSessions(wallet, client) {
  console.log("Initializing musig sessions");

  if (!musigRegistry) {
    console.warn("Musig system not initialized. Login required.");
    return;
  }

  try {
    console.log("Fetching entries from:", `${client.apiBase}/api/v1/entries`);

    const response = await client.get(`${client.apiBase}/api/v1/entries`);
    if (!response.ok) {
      throw new Error("Failed to fetch user entries");
    }

    const entries = await response.json();
    console.log("Retrieved entries:", entries);

    entries.sort((a, b) => a.id.localeCompare(b.id));

    // Start sessions for entries that need signing
    for (const entry of entries) {
      console.log("Checking entry:", entry);
      if (entry.public_nonces === null && entry.signed_at === null) {
        console.log("Entry needs signing:", entry);

        try {
          const entryIndex = entries.indexOf(entry);
          console.log(entryIndex);
          await musigRegistry.createSession(
            wallet,
            entry.event_id,
            entry.id,
            client,
            entryIndex,
          );
        } catch (error) {
          console.error("Error creating session for entry:", entry.id, error);
        }
      }
    }
  } catch (error) {
    console.error("Error initializing musig sessions:", error);
    throw error;
  }
}

document.addEventListener("DOMContentLoaded", async () => {
  registerPageReloadHandlers(apiBase, oracleBase);

  // Functions to open and close a modal
  function openModal($el) {
    $el.classList.add("is-active");
    document.documentElement.classList.add("is-clipped");
  }

  function closeModal($el) {
    $el.classList.remove("is-active");
    document.documentElement.classList.remove("is-clipped");
  }

  function closeAllModals() {
    (document.querySelectorAll(".modal") || []).forEach(($modal) => {
      closeModal($modal);
    });
  }

  function resetLoginModal() {
    // Clear any error messages
    const privateKeyError = document.querySelector("#privateKeyError");
    if (privateKeyError) privateKeyError.textContent = "";

    const extensionError = document.querySelector("#extensionLoginError");
    if (extensionError) extensionError.textContent = "";

    // Clear private key input
    document.getElementById("loginPrivateKey").value = "";

    // Reset to private key tab
    const privateKeyTab = document.querySelector(
      "#loginModal .tabs li[data-target='privateKeyLogin']",
    );
    if (privateKeyTab) {
      privateKeyTab.click();
    }
  }

  function resetRegisterModal() {
    // Clear registration error messages
    const extensionError = document.querySelector("#extensionRegisterError");
    if (extensionError) extensionError.textContent = "";

    // Clear private key display
    document.getElementById("privateKeyDisplay").value = "";

    // Uncheck saved checkbox
    document.getElementById("privateKeySavedCheckbox").checked = false;

    // Disable next button
    document.getElementById("registerStep1Button").disabled = true;

    // Reset to first step
    document.getElementById("registerStep1").classList.remove("is-hidden");
    document.getElementById("registerStep2").classList.add("is-hidden");

    // Reset to private key tab
    const privateKeyTab = document.querySelector(
      "#registerModal .tabs li[data-target='registerPrivateKey']",
    );
    if (privateKeyTab) {
      privateKeyTab.click();
    }
  }

  const auth_manager = await setupAuthManager(apiBase, network);

  document.getElementById("loginNavClick").addEventListener("click", () => {
    resetLoginModal();
    openModal(document.getElementById("loginModal"));
  });

  document.getElementById("registerNavClick").addEventListener("click", () => {
    // First reset the modal
    resetRegisterModal();
    // Then open it
    openModal(document.getElementById("registerModal"));
    // Then generate the private key
    auth_manager.handleRegisterStep1();
  });

  document.getElementById("closeLoginModal").addEventListener("click", () => {
    closeModal(document.getElementById("loginModal"));
  });

  document
    .getElementById("closeResisterModal")
    .addEventListener("click", () => {
      resetRegisterModal();
      closeModal(document.getElementById("registerModal"));
    });

  document
    .getElementById("showRegisterButton")
    .addEventListener("click", () => {
      let val = document.getElementById("loginModal");
      if (val) {
        closeModal(val);
      }
      resetRegisterModal();
      openModal(document.getElementById("registerModal"));
      auth_manager.handleRegisterStep1();
    });

  // Handle "Go to Login" button in registration
  document.getElementById("goToLoginButton").addEventListener("click", () => {
    let val = document.getElementById("registerModal");
    if (val) {
      closeModal(val);
    }
    openModal(document.getElementById("loginModal"));
  });

  const modalClosers = document.querySelectorAll(
    ".modal-background, .modal-close, .modal-card-head .delete, .modal-card-foot .button.is-cancel",
  );

  modalClosers.forEach(($close) => {
    const $target = $close.closest(".modal");
    $close.addEventListener("click", () => {
      closeModal($target);
    });
  });

  // Add a click event on buttons to open a specific modal
  (document.querySelectorAll(".js-modal-trigger") || []).forEach(($trigger) => {
    const modal = $trigger.dataset.target;
    const $target = document.getElementById(modal);

    $trigger.addEventListener("click", () => {
      openModal($target);
    });
  });

  // Add a keyboard event to close all modals
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      closeAllModals();
    }
  });

  const originalOnLoginSuccess = auth_manager.onLoginSuccess;
  auth_manager.onLoginSuccess = async function () {
    closeAllModals();

    // Initialize musig system after successful login
    musigRegistry = initializeMusigSystem();

    // Initialize musig sessions with the current wallet and client
    if (window.taprootWallet && this.authorizedClient) {
      try {
        await initializeMusigSessions(
          window.taprootWallet,
          this.authorizedClient,
        );
      } catch (error) {
        console.error("Failed to initialize musig sessions:", error);
      }
    }

    originalOnLoginSuccess.call(this);
  };

  await displayCompetitions(apiBase, oracleBase);
  showAllCompetition();
});

function showAllCompetition() {
  let $currentCompetitionCurrent = document.getElementById("allCompetitions");
  $currentCompetitionCurrent.classList.remove("hidden");
}
