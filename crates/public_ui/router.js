const isDevelopment = window.location.hostname === "localhost";
const UI_PREFIX = isDevelopment ? "/ui" : "";
const API_PREFIX = isDevelopment ? "" : "/api/v1";

class Router {
  constructor(routes) {
    this.routes = routes;
    this.currentPath = "";

    // Handle browser back/forward buttons
    window.addEventListener("popstate", () => {
      this.navigate(window.location.pathname, false);
    });

    // Handle direct URL access and refresh
    window.addEventListener("load", () => {
      this.init();
    });
  }

  init() {
    // Get the current path from the URL
    let path = window.location.pathname;

    // If it's the root path, default to competitions
    if (path === "/" || path === "") {
      path = "/competitions";
    }

    // Check if the path exists in our routes
    if (this.routes[path]) {
      this.navigate(path, false);
    } else {
      // Handle 404 or redirect to default route
      console.warn(`Route ${path} not found, redirecting to /competitions`);
      this.navigate("/competitions", true);
    }
  }

  navigate(path, addToHistory = true) {
    // Normalize the path
    path = path.startsWith("/") ? path : `/${path}`;

    if (addToHistory) {
      window.history.pushState({}, "", path);
    }

    this.currentPath = path;

    const route = this.routes[path];
    if (route) {
      try {
        route();
        this.updateNavigation(path);
      } catch (error) {
        console.error(`Error executing route ${path}:`, error);
        // Optionally handle route execution errors
      }
    } else {
      console.warn(`Route ${path} not found, redirecting to /competitions`);
      this.navigate("/competitions", true);
    }
  }

  updateNavigation(path) {
    document.querySelectorAll(".navbar-item[data-route]").forEach((item) => {
      item.classList.remove("is-active");
    });

    const currentNav = document.querySelector(`[data-route="${path}"]`);
    if (currentNav) {
      currentNav.classList.add("is-active");
    }
  }
}
export { Router };
