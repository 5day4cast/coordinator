// === shared/navbar.js ===
// Navbar burger toggle for mobile navigation (Bulma v1)

function setupNavbarBurger() {
    // Get all "navbar-burger" elements
    const navbarBurgers = Array.prototype.slice.call(
        document.querySelectorAll('.navbar-burger'),
        0
    );

    // Add a click event on each of them
    navbarBurgers.forEach((el) => {
        el.addEventListener('click', () => {
            // Get the target from the "data-target" attribute
            const targetId = el.dataset.target;
            const target = document.getElementById(targetId);

            if (!target) return;

            // Toggle the "is-active" class on both the "navbar-burger" and the "navbar-menu"
            el.classList.toggle('is-active');
            target.classList.toggle('is-active');

            // Update aria-expanded for accessibility
            const isExpanded = el.classList.contains('is-active');
            el.setAttribute('aria-expanded', isExpanded.toString());
        });
    });

    // Close navbar menu when clicking on a navbar item (for mobile UX)
    const navbarItems = document.querySelectorAll('.navbar-menu .navbar-item:not(.has-dropdown)');
    navbarItems.forEach((item) => {
        item.addEventListener('click', () => {
            const navbarMenu = document.querySelector('.navbar-menu');
            const navbarBurger = document.querySelector('.navbar-burger');

            if (navbarMenu && navbarBurger) {
                navbarMenu.classList.remove('is-active');
                navbarBurger.classList.remove('is-active');
                navbarBurger.setAttribute('aria-expanded', 'false');
            }
        });
    });

    // Close navbar menu when clicking outside (optional but good UX)
    document.addEventListener('click', (event) => {
        const navbarMenu = document.querySelector('.navbar-menu.is-active');
        const navbarBurger = document.querySelector('.navbar-burger.is-active');

        if (!navbarMenu || !navbarBurger) return;

        const isClickInsideNavbar = event.target.closest('.navbar');
        if (!isClickInsideNavbar) {
            navbarMenu.classList.remove('is-active');
            navbarBurger.classList.remove('is-active');
            navbarBurger.setAttribute('aria-expanded', 'false');
        }
    });
}

window.setupNavbarBurger = setupNavbarBurger;
