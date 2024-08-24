import { displayCompetitions } from "./competitions.js";
import { displayEntries } from "./entries.js";
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
    console.log("click", event);
    event.preventDefault();
    const targetContainerId = this.id.replace("NavClick", "");
    console.log("in nav click ", targetContainerId);
    //router
    switch (targetContainerId) {
      case "logout":
        console.log("logging out");
        document.dispatchEvent(new Event("nlLogout"));
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
  if ($containerToShow) {
    $containerToShow.classList.remove("hidden");
  }
}
