import { displayCompetitions } from "./competitions.js";
import { displayEntries } from "./entries.js";
import { displayPayouts } from "./payouts.js";
import { getMusigRegistry } from "./main.js";
import { Router } from "./router.js";

const oracleBase = ORACLE_BASE;
const apiBase = API_BASE;

function registerPageReloadHandlers(apiBase, oracleBase) {
  const pages = {
    allCompetitions: () => displayCompetitions(apiBase, oracleBase),
    entriesContainer: () => displayEntries(apiBase, oracleBase),
    payoutsContainer: () => displayPayouts(apiBase, oracleBase),
  };

  document.querySelectorAll("[data-container]").forEach((element) => {
    element.addEventListener("click", () => {
      const containerId = element.getAttribute("data-container");
      if (pages[containerId]) {
        pages[containerId]();
      }
    });
  });
}

const routes = {
  "/competitions": () => {
    hideAllContainers();
    showContainer("allCompetitions");
    displayCompetitions(apiBase, oracleBase);
  },
  "/entries": () => {
    // Check if user is authenticated
    if (!window.taprootWallet) {
      router.navigate("/competitions");
      // Optionally show login modal
      const loginModal = document.getElementById("loginModal");
      if (loginModal) {
        loginModal.classList.add("is-active");
        document.documentElement.classList.add("is-clipped");
      }
      return;
    }
    hideAllContainers();
    showContainer("allEntries");
    displayEntries(apiBase, oracleBase);
  },
  "/signing": () => {
    if (!window.taprootWallet) {
      router.navigate("/competitions");
      return;
    }
    hideAllContainers();
    showContainer("signingStatus");
    const registry = getMusigRegistry();
    if (registry) {
      const observers = Array.from(registry.observers);
      const signingUI = observers.find((obs) => obs.toggleVisibility);
      if (signingUI) {
        signingUI.show();
        signingUI.updateUI();
      }
    }
  },
  "/payouts": () => {
    if (!window.taprootWallet) {
      router.navigate("/competitions");
      // Optionally show login modal
      const loginModal = document.getElementById("loginModal");
      if (loginModal) {
        loginModal.classList.add("is-active");
        document.documentElement.classList.add("is-clipped");
      }
      return;
    }
    hideAllContainers();
    showContainer("payouts");
    displayPayouts(apiBase, oracleBase);
  },
};

// Initialize router
const router = new Router(routes);

document.addEventListener("DOMContentLoaded", () => {
  // Mobile menu handling
  const $navbarBurgers = Array.prototype.slice.call(
    document.querySelectorAll(".navbar-burger"),
    0,
  );

  $navbarBurgers.forEach((el) => {
    el.addEventListener("click", () => {
      const target = el.dataset.target;
      const $target = document.getElementById(target);
      el.classList.toggle("is-active");
      $target.classList.toggle("is-active");
    });
  });

  // Handle navigation clicks
  document
    .querySelectorAll(".navbar-item[data-route]")
    .forEach(($navbarItem) => {
      $navbarItem.addEventListener("click", (event) => {
        event.preventDefault();

        // Close mobile menu if open
        const $navbarMenu = document.querySelector(".navbar-menu");
        const $burger = document.querySelector(".navbar-burger");
        if ($navbarMenu.classList.contains("is-active")) {
          $navbarMenu.classList.remove("is-active");
          $burger.classList.remove("is-active");
        }

        // Get route from data attribute
        const route = $navbarItem.dataset.route;
        if (route) {
          router.navigate(route);
        }
      });
    });

  // Initialize router
  router.init();
});

function hideAllContainers() {
  const containers = [
    "allCompetitions",
    "allEntries",
    "signingStatus",
    "payouts",
  ];
  containers.forEach((containerId) => {
    const container = document.getElementById(containerId);
    if (container) {
      container.classList.add("hidden");
    }
  });
}

function showContainer(containerId) {
  const container = document.getElementById(containerId);
  if (container) {
    container.classList.remove("hidden");
  }
}

export { hideAllContainers, showContainer, registerPageReloadHandlers };
