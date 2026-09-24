// Signs htmx requests with a NIP-98 header from the WASM wallet, so account
// pages can be rendered on the server. The key never leaves WASM; every
// request gets its own short-lived signature over its exact URL.

// Pages that need an account, and pages that personalize when they have one.
const AUTH_REQUIRED = [/^\/entries$/, /^\/payouts$/];
const AUTH_OPTIONAL = [/^\/competitions\/[^/]+\/entry-form(\/payout)?$/];

function authMode(url) {
  const path = new URL(url, window.location.origin).pathname;
  if (AUTH_REQUIRED.some((pattern) => pattern.test(path))) return "required";
  if (AUTH_OPTIONAL.some((pattern) => pattern.test(path))) return "optional";
  return null;
}

function isLoggedIn() {
  return Boolean(window.nostrClient?.isSignerReady?.() && window.dlcWallet);
}

async function generateAuthHeader(method, url) {
  if (!isLoggedIn()) return null;
  const fullUrl = new URL(url, window.location.origin).href;
  return window.nostrClient.getAuthHeader(fullUrl, method, null);
}

function showAuthError(message) {
  const notification = document.createElement("div");
  notification.className = "notification is-danger auth-error";
  const close = document.createElement("button");
  close.className = "delete";
  close.addEventListener("click", () => notification.remove());
  const title = document.createElement("strong");
  title.textContent = "Authentication Error";
  // textContent, not innerHTML: messages can carry error text from elsewhere.
  notification.append(close, title, document.createElement("br"), message);
  document.body.appendChild(notification);
  setTimeout(() => notification.remove(), 5000);
}

// A navigation that needed a login, repeated once the login succeeds.
let waitingForLogin = null;

function setupHtmxAuth() {
  // htmx:confirm lets the request wait for the asynchronous signature.
  document.body.addEventListener("htmx:confirm", async (event) => {
    const { verb, path, elt } = event.detail;
    const mode = authMode(path);
    if (!mode || (mode === "optional" && !isLoggedIn())) return;

    event.preventDefault();
    if (!isLoggedIn()) {
      // Account pages opened by a click: log in, then carry on.
      waitingForLogin = elt;
      window.openAuthModal?.("loginModal");
      return;
    }
    try {
      elt._pendingAuthHeader = await generateAuthHeader(verb, path);
      event.detail.issueRequest();
    } catch (error) {
      console.error("Failed to sign request for", path, error);
      showAuthError("Could not sign the request. Please log in again.");
    }
  });

  document.body.addEventListener("htmx:configRequest", (event) => {
    const elt = event.detail.elt;
    if (elt._pendingAuthHeader) {
      event.detail.headers["Authorization"] = elt._pendingAuthHeader;
      delete elt._pendingAuthHeader;
    }
  });

  document.body.addEventListener("fw:login", () => {
    const elt = waitingForLogin;
    waitingForLogin = null;
    if (elt?.isConnected) elt.click();
  });

  document.body.addEventListener("htmx:responseError", (event) => {
    if (event.detail.xhr.status === 401) {
      showAuthError("Your session ended. Please log in again.");
      window.openAuthModal?.("loginModal");
    }
  });
}

window.isLoggedIn = isLoggedIn;
window.generateAuthHeader = generateAuthHeader;
window.setupHtmxAuth = setupHtmxAuth;
