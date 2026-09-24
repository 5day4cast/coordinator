// Signs account pages' htmx requests with a NIP-98 header from the WASM
// wallet, so they can be rendered on the server. The key never leaves WASM;
// each signature is single-use, lasts 60 s and names one method and URL.
//
// - Only GETs are signed here. Account changes go through AuthorizedClient,
//   which also signs the hash of the exact body it sends.
// - A signature is bound to the method and URL it was made for, and is only
//   attached if htmx sends exactly that URL, query string included.
// - A signature that goes unused (cancelled, failed, stale) is dropped.
// - Public pages, including the leaderboard's and picks' refreshes, are never
//   signed: polling them must not ask a Nostr extension to sign every minute.

// Pages that need an account, and pages that personalize when they have one.
const AUTH_REQUIRED = [/^\/entries$/, /^\/payouts$/];
const AUTH_OPTIONAL = [/^\/competitions\/[^/]+\/entry-form(\/payout)?$/];

// Unused signatures are dropped well before the server's 60 s window ends.
const SIGNATURE_LIFETIME_MS = 30_000;

function authMode(url) {
  const path = new URL(url, window.location.origin).pathname;
  if (AUTH_REQUIRED.some((pattern) => pattern.test(path))) return "required";
  if (AUTH_OPTIONAL.some((pattern) => pattern.test(path))) return "optional";
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

// Request continuations and single-use signatures are scoped to their source element.
const continuations = new WeakMap();
const pending = new WeakMap();
const sending = new WeakMap();

// Match htmx's GET serialization, including repeated values and existing queries.
// validateUrl below checks this against htmx's actual final URL before sending.
function signedGetUrl(detail) {
  let path = detail.path.split("#")[0];
  const pairs = [];
  for (const [name, values] of Object.entries(detail.parameters)) {
    for (let value of Array.isArray(values) ? values : [values]) {
      if (typeof value === "object") value = JSON.stringify(value);
      pairs.push(`${encodeURIComponent(name)}=${encodeURIComponent(value)}`);
    }
  }
  if (pairs.length) path += `${path.includes("?") ? "&" : "?"}${pairs.join("&")}`;
  return new URL(path, window.location.href || window.location.origin).href;
}

function forgetSignature(event) {
  const elt = event.detail?.elt ?? event.target;
  pending.delete(elt);
  sending.delete(elt);
  continuations.delete(elt);
}

let waitingForLogin = null;

function setupHtmxAuth() {
  // Clear snapshots left by an older release before Back can restore one.
  try { localStorage.removeItem("htmx-history-cache"); } catch (_) {}
  document.body.addEventListener("htmx:confirm", (event) => {
    const { verb, path, elt } = event.detail;
    const mode = authMode(path);
    if (!mode || (mode === "optional" && !isLoggedIn())) return;
    if (verb !== "get") {
      event.preventDefault();
      console.error(`Not signing an htmx ${verb.toUpperCase()} to ${path}: use AuthorizedClient`);
      return;
    }
    if (!isLoggedIn()) {
      event.preventDefault();
      waitingForLogin = elt;
      window.openAuthModal?.("loginModal");
      return;
    }
    continuations.set(elt, event.detail.issueRequest);
  });

  // Parameters (hx-include / hx-vals / forms) are only final at configRequest.
  // Cancel this pass while signing, then repeat it with the single-use header.
  document.body.addEventListener("htmx:configRequest", (event) => {
    const { elt, verb } = event.detail;
    const mode = authMode(event.detail.path);
    if (!mode || (mode === "optional" && !isLoggedIn())) return;
    if (verb !== "get" || !isLoggedIn()) {
      event.preventDefault();
      return;
    }
    const url = signedGetUrl(event.detail);
    if (new URL(url).origin !== window.location.origin) {
      event.preventDefault();
      forgetSignature(event);
      return;
    }
    const signature = pending.get(elt);
    if (signature?.header) {
      pending.delete(elt);
      continuations.delete(elt);
      if (signature.url !== url || signature.verb !== verb || Date.now() - signature.at > SIGNATURE_LIFETIME_MS) {
        event.preventDefault();
        return;
      }
      event.detail.headers.Authorization = signature.header;
      sending.set(elt, url);
      Promise.resolve().then(() => {
        if (event.defaultPrevented && sending.get(elt) === url) sending.delete(elt);
      });
      return;
    }
    event.preventDefault();
    if (signature) return; // One signing request per element at a time.
    const resume = continuations.get(elt);
    if (!resume) return;
    const token = { verb, url, at: Date.now(), header: null, signer: session.nostrClient };
    pending.set(elt, token);
    session.nostrClient.getAuthHeader(url, "GET", null).then((header) => {
      if (pending.get(elt) !== token) return;
      if (!isLoggedIn() || token.signer !== session.nostrClient || elt.isConnected === false) {
        forgetSignature({ detail: { elt } });
        return;
      }
      token.header = header;
      resume(true);
    }).catch((error) => {
      if (pending.get(elt) === token) forgetSignature({ detail: { elt } });
      console.error("Failed to sign request", error);
      showAuthError("Could not sign the request. Please log in again.");
    });
  });

  // The URL htmx is about to open, with any parameters it added. A request to
  // any other URL than the signed one is not sent.
  document.body.addEventListener("htmx:validateUrl", (event) => {
    const elt = event.detail.elt ?? event.target;
    const signedUrl = sending.get(elt);
    if (signedUrl === undefined) return;
    sending.delete(elt);
    // A fragment identifies a position in the document; it is never sent in HTTP.
    const actualUrl = new URL(event.detail.url.href);
    actualUrl.hash = "";
    if (actualUrl.href !== signedUrl) {
      event.preventDefault();
      console.error(`Not sending the signature for ${signedUrl} to ${actualUrl.href}`);
    }
  });

  for (const name of [
    "htmx:afterRequest",
    "htmx:sendError",
    "htmx:timeout",
    "htmx:xhr:abort",
    "htmx:abort",
    "htmx:validation:halted",
    "htmx:beforeCleanupElement",
    "htmx:invalidPath",
  ]) {
    document.body.addEventListener(name, forgetSignature);
  }

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

  // htmx keeps no history snapshots (historyCacheSize 0), so Back reloads the
  // page from the server, unsigned: an account page comes back as the log-in
  // prompt, the entry form without the account's payout address. While still
  // logged in, those parts load again, signed.
  document.body.addEventListener("htmx:historyRestore", () => {
    if (!isLoggedIn()) return;
    for (const element of document.querySelectorAll("[data-signed-reload]")) {
      window.htmx?.trigger(element, "fw:reload");
    }
  });
}

window.setupHtmxAuth = setupHtmxAuth;
