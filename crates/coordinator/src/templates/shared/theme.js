// === shared/theme.js ===
// Dark/Light mode toggle with localStorage persistence
// Uses Bulma v1's data-theme attribute for theming

const THEME_STORAGE_KEY = 'fantasy-weather-theme';
const THEME_DARK = 'dark';
const THEME_LIGHT = 'light';

/**
 * Get the user's preferred theme from:
 * 1. localStorage (explicit user choice)
 * 2. System preference (prefers-color-scheme)
 * 3. Default to light
 */
function getPreferredTheme() {
    // Check localStorage first (user's explicit choice)
    const stored = localStorage.getItem(THEME_STORAGE_KEY);
    if (stored === THEME_DARK || stored === THEME_LIGHT) {
        return stored;
    }

    // Check system preference
    if (window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches) {
        return THEME_DARK;
    }

    return THEME_LIGHT;
}

/**
 * Apply the theme to the document
 */
function applyTheme(theme) {
    document.documentElement.setAttribute('data-theme', theme);

    // Update the toggle button's appearance
    updateToggleButton(theme);
}

/**
 * Update the toggle button to show the correct icon
 */
function updateToggleButton(theme) {
    const lightIcon = document.querySelector('.theme-icon-light');
    const darkIcon = document.querySelector('.theme-icon-dark');

    if (lightIcon && darkIcon) {
        if (theme === THEME_DARK) {
            // In dark mode: show sun icon (to switch to light)
            lightIcon.style.display = 'inline-flex';
            darkIcon.style.display = 'none';
        } else {
            // In light mode: show moon icon (to switch to dark)
            lightIcon.style.display = 'none';
            darkIcon.style.display = 'inline-flex';
        }
    }
}

/**
 * Toggle between dark and light themes
 */
function toggleTheme() {
    const currentTheme = document.documentElement.getAttribute('data-theme') || THEME_LIGHT;
    const newTheme = currentTheme === THEME_DARK ? THEME_LIGHT : THEME_DARK;

    // Save to localStorage
    localStorage.setItem(THEME_STORAGE_KEY, newTheme);

    // Apply the new theme
    applyTheme(newTheme);
}

/**
 * Initialize theme system
 */
function setupThemeToggle() {
    // Apply preferred theme immediately
    const preferredTheme = getPreferredTheme();
    applyTheme(preferredTheme);

    // Set up toggle button click handler
    const toggleButton = document.getElementById('themeToggle');
    if (toggleButton) {
        toggleButton.addEventListener('click', toggleTheme);
    }

    // Listen for system theme changes (if user hasn't set explicit preference)
    if (window.matchMedia) {
        window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', (e) => {
            // Only auto-switch if user hasn't set an explicit preference
            if (!localStorage.getItem(THEME_STORAGE_KEY)) {
                applyTheme(e.matches ? THEME_DARK : THEME_LIGHT);
            }
        });
    }
}

// Export for use in base.js
window.setupThemeToggle = setupThemeToggle;
window.toggleTheme = toggleTheme;
