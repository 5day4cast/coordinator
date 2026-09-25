// Signs account pages' htmx requests with a NIP-98 header from the WASM
// wallet, so they can be rendered on the server. The key never leaves WASM;
// each signature is single-use, lasts 60 s and names one method and URL.
//
// - Only GETs are signed here. Account changes go through AuthorizedClient,
//   which also signs the hash of the exact body it sends.
// - htmx 4 builds the final URL, query string included, before
//   htmx:before:request and then calls ctx.fetch(url, request). The signature
//   is made inside that call, for exactly the URL and method being sent.
// - Public pages, including the leaderboard's and picks' refreshes, are never
//   signed: polling them must not ask a Nostr extension to sign every minute.
//
// It is an htmx extension, registered when this script loads, so it is in
// place before htmx sends anything and page scripts cannot run ahead of it.

// Pages that need an account, and pages that personalize when they have one.
const AUTH_REQUIRED = [
  /^\/entries$/,
  /^\/payouts$/,
  /^\/competitions\/[^/]+\/tickets\/[^/]+\/status$/,
];
const AUTH_OPTIONAL = [/^\/competitions\/[^/]+\/entry-form(\/payout)?$/];

function authMode(url) {
  if (AUTH_REQUIRED.some((pattern) => pattern.test(url.pathname))) return "required";
  if (AUTH_OPTIONAL.some((pattern) => pattern.test(url.pathname))) return "optional";
  return null;
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

const SIGNED_REQUEST_TIMEOUT_MS = 60_000;

// The element whose account request waits for the visitor to log in.
let waitingForLogin = null;

// The fetch htmx will call, wrapped to add the signature for `url`.
function signedFetch(send, url, signer) {
  return async (action, request) => {
    const sent = new URL(action, document.baseURI);
    // A fragment identifies a position in the document; it is never sent in HTTP.
    sent.hash = "";
    if (sent.href !== url.href || request.method !== "GET") {
      throw new Error(`Not signing ${request.method} ${sent.href}`);
    }
    let authorization;
    try {
      authorization = await signer.getAuthHeader(url.href, "GET", null);
    } catch (error) {
      showAuthError("Could not sign the request. Please log in again.");
      throw error;
    }
    // Logged out, or into another account, while the wallet was signing.
    if (!isLoggedIn() || session.nostrClient !== signer) {
      throw new Error("The account changed while signing");
    }
    return send(action, { ...request, headers: { ...request.headers, Authorization: authorization } });
  };
}

window.htmx?.registerExtension("fw-auth", {
  htmx_before_request(elt, detail) {
    const ctx = detail.ctx;
    const url = new URL(ctx.request.action, document.baseURI);
    url.hash = "";
    const mode = authMode(url);
    if (!mode) return;
    if (!isLoggedIn()) {
      // Optional pages render for everyone; a history restore of an account
      // page comes back from the server as its log-in prompt.
      if (mode === "optional" || ctx.request.headers["HX-History-Restore-Request"]) return;
      waitingForLogin = elt;
      openAuthModal("loginModal");
      return false;
    }
    if (ctx.request.method !== "GET" || url.origin !== window.location.origin) {
      console.error(`Not signing an htmx ${ctx.request.method} to ${url.href}: use AuthorizedClient`);
      return false;
    }
    ctx.fetch = signedFetch(ctx.fetch, url, session.nostrClient);
    // A Nostr extension may ask the player to approve the signature, which
    // can take longer than the page's usual 10 s request timeout.
    clearTimeout(ctx.requestTimeout);
    ctx.requestTimeout = setTimeout(() => ctx.request.abort?.(), SIGNED_REQUEST_TIMEOUT_MS);
  },

  htmx_response_error(elt, detail) {
    if (detail.ctx.response.status === 401) {
      showAuthError("Your session ended. Please log in again.");
      openAuthModal("loginModal");
    }
  },
});

function setupHtmxAuth() {
  // htmx 1.x kept page snapshots in localStorage, account pages included.
  // htmx 4 keeps none; clear any an older release left behind.
  try { localStorage.removeItem("htmx-history-cache"); } catch (_) {}

  document.body.addEventListener("fw:login", () => {
    const elt = waitingForLogin;
    waitingForLogin = null;
    if (elt?.isConnected) elt.click();
  });
}

