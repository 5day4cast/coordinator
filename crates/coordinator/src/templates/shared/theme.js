// Dark/light toggle. The page's <head> applies the saved or system theme
// before first paint; this keeps it in step with the toggle and the system.
const THEME_STORAGE_KEY = "fantasy-weather-theme";

function applyTheme(theme) {
  document.documentElement.setAttribute("data-theme", theme);
}

function setupThemeToggle() {
  document.getElementById("themeToggle")?.addEventListener("click", () => {
    const next = document.documentElement.getAttribute("data-theme") === "dark" ? "light" : "dark";
    localStorage.setItem(THEME_STORAGE_KEY, next);
    applyTheme(next);
  });
  window.matchMedia?.("(prefers-color-scheme: dark)").addEventListener("change", (event) => {
    if (!localStorage.getItem(THEME_STORAGE_KEY)) applyTheme(event.matches ? "dark" : "light");
  });
}

