// The phone menu: the burger opens it; following a link or clicking
// elsewhere closes it.
function setupNavbarBurger() {
  const burger = document.querySelector(".navbar-burger");
  const menu = burger && document.getElementById(burger.dataset.target);
  if (!menu) return;
  const setOpen = (open) => {
    burger.classList.toggle("is-active", open);
    menu.classList.toggle("is-active", open);
    burger.setAttribute("aria-expanded", String(open));
  };
  burger.addEventListener("click", () => setOpen(!menu.classList.contains("is-active")));
  document.addEventListener("click", (event) => {
    if (!menu.classList.contains("is-active") || event.target.closest(".navbar-burger")) return;
    if (!event.target.closest(".navbar") || event.target.closest(".navbar-item")) setOpen(false);
  });
}

window.setupNavbarBurger = setupNavbarBurger;
