// Applies the saved or system theme. The page loads this in <head> without
// defer, so it never paints in the wrong theme; shared/theme.js runs the toggle.
document.documentElement.dataset.theme =
  localStorage.getItem("fantasy-weather-theme") ||
  (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
