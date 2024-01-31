
const $navDivs = document.querySelectorAll('a[id$="NavClick"]');
const $navbarItems = document.querySelectorAll('.navbar-item');
const $navbarBurgers = Array.prototype.slice.call(document.querySelectorAll('.navbar-burger'), 0);

// Add a click event on each of them
$navbarBurgers.forEach(el => {
    el.addEventListener('click', () => {

        // Get the target from the "data-target" attribute
        const target = el.dataset.target;
        const $target = document.getElementById(target);

        // Toggle the "is-active" class on both the "navbar-burger" and the "navbar-menu"
        el.classList.toggle('is-active');
        $target.classList.toggle('is-active');

    });
});

$navbarItems.forEach(function ($navbarItem) {
    $navbarItem.addEventListener('click', function (event) {
        event.preventDefault();
        // Hide all containers
        hideAllContainers();
        // Extract the ID from the clicked navbar item
        const targetContainerId = this.id.replace('NavClick', '');
        // Show the corresponding container
        console.log(targetContainerId);
        showContainer(targetContainerId);
    });
});

// Function to hide all containers
export function hideAllContainers() {
    $navDivs.forEach(function ($container) {
        const containerId = $container.id.split("NavClick")[0];
        const $containerToHide = document.getElementById(containerId);
        if ($containerToHide) {
            $containerToHide.classList.add('hidden');
        }
    });
}

// Function to show a specific container
export function showContainer(containerId) {
    const $containerToShow = document.getElementById(containerId);
    if ($containerToShow) {
        $containerToShow.classList.remove('hidden');
    }
}