import { displayCompetitions } from "./competitions.js";
import { displayEntries } from "./entries.js";
import { getMusigRegistry } from "./main.js";

const oracleBase = ORACLE_BASE;
const apiBase = API_BASE;

const $navDivs = document.querySelectorAll('a[id$="NavClick"]');
const $navbarItems = document.querySelectorAll(".navbar-item");
const $navbarBurgers = Array.prototype.slice.call(
  document.querySelectorAll(".navbar-burger"),
  0,
);

// Add a click event on each of them
$navbarBurgers.forEach((el) => {
  el.addEventListener("click", () => {
    // Get the target from the "data-target" attribute
    const target = el.dataset.target;
    const $target = document.getElementById(target);

    // Toggle the "is-active" class on both the "navbar-burger" and the "navbar-menu"
    el.classList.toggle("is-active");
    $target.classList.toggle("is-active");
  });
});

$navbarItems.forEach(function ($navbarItem) {
  $navbarItem.addEventListener("click", function (event) {
    event.preventDefault();
    const targetContainerId = this.id.replace("NavClick", "");
    //router
    switch (targetContainerId) {
      case "logout":
        console.log("logging out");
        break;
      case "allEntries":
        console.log("displaying entries");
        hideAllContainers();
        showContainer(targetContainerId);
        displayEntries(apiBase, oracleBase);
        break;
      case "allCompetitions":
        console.log("displaying competitions");
        hideAllContainers();
        showContainer(targetContainerId);
        displayCompetitions(apiBase, oracleBase);
        break;
      case "signingStatus":
        console.log("displaying signing status");
        hideAllContainers();
        showContainer(targetContainerId);
        const registry = getMusigRegistry();
        if (registry) {
          const observers = Array.from(registry.observers);
          console.log("Observers:", observers);
          const signingUI = observers.find((obs) => obs.toggleVisibility);
          console.log("SigningUI:", signingUI);
          if (signingUI) {
            signingUI.show();
            signingUI.updateUI();
          }
        }
        break;
      case "payouts":
        console.log("displaying payouts");
        hideAllContainers();
        showContainer(targetContainerId);
        displayPayouts(apiBase, oracleBase);
        break;
      default:
    }
  });
});
function hideAllContainers() {
  $navDivs.forEach(function ($container) {
    const containerId = $container.id.split("NavClick")[0];
    const $containerToHide = document.getElementById(containerId);
    if ($containerToHide) {
      $containerToHide.classList.add("hidden");
    }
  });
}

function showContainer(containerId) {
  const $containerToShow = document.getElementById(containerId);
  console.log("Showing container:", containerId, $containerToShow);

  if ($containerToShow) {
    $containerToShow.classList.remove("hidden");
    console.log(
      "Container classes after show:",
      $containerToShow.classList.toString(),
    );
  }
}

export { hideAllContainers, showContainer };
