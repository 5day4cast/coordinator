import { displayCompetitions } from "./competitions.js";
import { init as initNostrLogin } from "https://www.unpkg.com/nostr-login@latest/dist/index.esm.js";
const apiBase = API_BASE;
console.log("api location:", apiBase);
const oracleBase = ORACLE_BASE;
console.log("oracle location:", oracleBase);

initNostrLogin({
  isSignInWithExtension: true,
  perms: "sign_event:1,nip04_encrypt",
  theme: "ocean",
});

document.dispatchEvent(new CustomEvent("nlLaunch", { detail: "welcome" }));

document.addEventListener("nlAuth", (e) => {
  console.log(e);
  // type is login, signup or logout
  if (e.detail.type === "login" || e.detail.type === "signup") {
    onLogin();
  } else {
    onLogout();
    //TODO: set background to blank
  }
});

function onLogin() {
  if (window.nostr) {
    displayCompetitions(apiBase, oracleBase);
    showAllCompetition();
  }
}

function showAllCompetition() {
  let $currentCompetitionCurrent = document.getElementById("allCompetitions");
  $currentCompetitionCurrent.classList.remove("hidden");
}

function onLogout() {
  console.log(window.nostr);
  document.dispatchEvent(new CustomEvent("nlLaunch", { detail: "welcome" }));
}

document.addEventListener("DOMContentLoaded", () => {
  // Functions to open and close a modal
  function openModal($el) {
    $el.classList.add("is-active");
  }

  function closeModal($el) {
    $el.classList.remove("is-active");
  }

  function closeAllModals() {
    (document.querySelectorAll(".modal") || []).forEach(($modal) => {
      closeModal($modal);
    });
  }

  // Add a click event on buttons to open a specific modal
  (document.querySelectorAll(".js-modal-trigger") || []).forEach(($trigger) => {
    const modal = $trigger.dataset.target;
    const $target = document.getElementById(modal);

    $trigger.addEventListener("click", () => {
      openModal($target);
    });
  });

  // Add a click event on various child elements to close the parent modal
  (
    document.querySelectorAll(
      ".modal-background, .modal-close, .modal-card-head .delete, .modal-card-foot .button",
    ) || []
  ).forEach(($close) => {
    const $target = $close.closest(".modal");

    $close.addEventListener("click", () => {
      closeModal($target);
    });
  });

  // Add a keyboard event to close all modals
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      closeAllModals();
    }
  });
});
