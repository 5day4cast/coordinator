const AUTH_REQUIRED_ROUTES = ["/entries", "/payouts", "/entry-form"];
const PUBLIC_ROUTES = ["/entries/", "/detail"]; // Entry detail pages are public (leaderboard)

function requiresAuth(url) {
  // Entry detail routes are public (accessed from leaderboard)
  if (url.includes("/entries/") && url.includes("/detail")) {
    return false;
  }
  return AUTH_REQUIRED_ROUTES.some((route) => url.includes(route));
}

function isLoggedIn() {
  // Check that nostrClient exists, has an initialized signer, and taprootWallet exists
  return (
    window.nostrClient &&
    typeof window.nostrClient.isSignerReady === "function" &&
    window.nostrClient.isSignerReady() &&
    window.taprootWallet
  );
}

async function generateAuthHeader(method, url) {
  if (!isLoggedIn()) return null;

  try {
    const fullUrl = new URL(url, window.location.origin).href;
    return await window.nostrClient.getAuthHeader(fullUrl, method, null);
  } catch (error) {
    console.error("Failed to generate auth header:", error);
    return null;
  }
}

function showAuthError(message) {
  // Show a user-visible notification that auth failed
  const notification = document.createElement("div");
  notification.className = "notification is-danger";
  notification.style.cssText =
    "position: fixed; top: 20px; right: 20px; z-index: 9999; max-width: 400px;";
  notification.innerHTML = `
    <button class="delete" onclick="this.parentElement.remove()"></button>
    <strong>Authentication Error</strong><br>
    ${message}
  `;
  document.body.appendChild(notification);

  // Auto-remove after 5 seconds
  setTimeout(() => notification.remove(), 5000);
}

function setupHtmxAuth() {
  // Use htmx:confirm for async auth header generation
  // This event allows us to call issueRequest() after async work completes
  document.body.addEventListener("htmx:confirm", async (event) => {
    const { verb, path } = event.detail;

    // If route doesn't require auth, let HTMX proceed normally
    if (!requiresAuth(path)) return;

    // If user is not logged in, show login modal instead of making request
    if (!isLoggedIn()) {
      event.preventDefault();
      const loginModal = document.getElementById("loginModal");
      if (loginModal) {
        loginModal.classList.add("is-active");
      }
      return;
    }

    // User is logged in, need to generate auth header
    event.preventDefault();

    try {
      const authHeader = await generateAuthHeader(verb, path);

      if (authHeader) {
        event.detail.elt._pendingAuthHeader = authHeader;
        event.detail.issueRequest();
      } else {
        // Auth header generation returned null - don't make the request
        console.error("HTMX auth: Failed to generate auth header for", path);
        showAuthError(
          "Failed to authenticate request. Please try logging in again.",
        );
      }
    } catch (error) {
      console.error(
        "HTMX auth: Exception during auth header generation:",
        error,
      );
      showAuthError("Authentication error: " + error.message);
    }
  });

  // Synchronously apply the pre-generated header
  document.body.addEventListener("htmx:configRequest", (event) => {
    const elt = event.detail.elt;
    if (elt._pendingAuthHeader) {
      event.detail.headers["Authorization"] = elt._pendingAuthHeader;
      delete elt._pendingAuthHeader;
    }
  });

  // Handle auth errors from server responses
  document.body.addEventListener("htmx:responseError", (event) => {
    if (event.detail.xhr.status === 401) {
      showAuthError("Session expired. Please log in again.");
    }
  });
}

window.requiresAuth = requiresAuth;
window.isLoggedIn = isLoggedIn;
window.generateAuthHeader = generateAuthHeader;
window.setupHtmxAuth = setupHtmxAuth;
