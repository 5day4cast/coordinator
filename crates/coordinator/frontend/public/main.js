import { displayCompetitions } from "./competitions.js";
import { setupAuthManager } from "./auth_manager.js";
import { registerPageReloadHandlers } from "./navbar.js";

const apiBase = API_BASE;
console.log("api location:", apiBase);
const oracleBase = ORACLE_BASE;
console.log("oracle location:", oracleBase);

const network = NETWORK;
console.log("bitcoin network:", network);

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
    // Keymeld handles signing - no local MuSig initialization needed
    originalOnLoginSuccess.call(this);
  };

  await displayCompetitions(apiBase, oracleBase);
  showAllCompetition();
});

function showAllCompetition() {
  let $currentCompetitionCurrent = document.getElementById("allCompetitions");
  $currentCompetitionCurrent.classList.remove("hidden");
}
