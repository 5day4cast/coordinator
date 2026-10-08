// Satchel, a Lightning wallet for test networks, when this site names one
// (ui_settings.satchel_url, the page's data-satchel-url). Its links carry the
// Satchel page to open in data-satchel-next: "Pay with Satchel" the invoice,
// "Open Satchel" the wallet.
//
// - With the player's key in this page (a password login or a Nostr extension),
//   a click signs the player in there: the page signs a NIP-98 event for
//   Satchel's handoff URL and posts it to Satchel in a tab of its own. Satchel
//   signs them in, or offers to make them a wallet under their username here.
// - Without it, the link itself opens Satchel, which asks them to sign in.
// - On the Payouts page, Satchel says whether the player has a wallet there, by
//   a signed GET; if so, its Lightning Address is offered for the address field.
//
// The page's CSP allows Satchel's origin as a form target and to connect to,
// only when it is configured (api/public_headers.rs).

// The tab Satchel opens in, reused by later clicks.
const SATCHEL_TAB = "satchel";

function satchelOrigin() {
  return document.body?.dataset.satchelUrl || "";
}

// The signer the player is logged in with, when there is one.
function satchelSigner() {
  const signer = session.nostrClient;
  return signer?.isSignerReady?.() ? signer : null;
}

// The player's username here, suggested to Satchel for a new wallet; an account
// made with a Nostr extension has none. Asked once per login.
let satchelUsername = { signer: null, name: null };

async function satchelName(signer) {
  if (satchelUsername.signer === signer) return satchelUsername.name;
  const base = document.body.dataset.apiBase || "";
  try {
    const response = await new AuthorizedClient(signer, base).post(`${base}/api/v1/users/login`);
    const { username } = await response.json();
    satchelUsername = { signer, name: typeof username === "string" && username ? username : null };
    return satchelUsername.name;
  } catch (error) {
    // Satchel then suggests no name.
    console.warn("Could not look up the username for Satchel:", error);
    return null;
  }
}

// A top-level form POST of the signed event to Satchel's handoff, into its tab.
function postHandoff(origin, signedEvent, next, target = SATCHEL_TAB) {
  const form = document.createElement("form");
  form.method = "post";
  form.action = `${origin}/auth/nostr/handoff`;
  form.target = target;
  form.hidden = true;
  for (const [name, value] of [["event", signedEvent], ["next", next]]) {
    const input = document.createElement("input");
    input.type = "hidden";
    input.name = name;
    input.value = value;
    form.append(input);
  }
  document.body.append(form);
  form.submit();
  form.remove();
}

// A click on a Satchel link, signed in as the player when this page holds their key.
async function handOffToSatchel(event) {
  const link = event.target.closest?.("a[data-satchel-next]");
  const origin = satchelOrigin();
  const signer = satchelSigner();
  const next = link?.dataset.satchelNext;
  if (!link || !origin || !next) return;
  const signup = link.dataset.signupSatchel !== undefined;
  const status = signup ? link.parentElement?.querySelector("[data-satchel-status]") : null;
  if (!signer) {
    if (signup) {
      event.preventDefault();
      if (status) status.textContent = "Your signup session ended. Start signup again to connect this wallet.";
    }
    return;
  }
  event.preventDefault();
  // Opened now, inside the click, so no popup blocker stops it; the form fills it once signed.
  const tab = window.open("", SATCHEL_TAB);
  if (status) status.textContent = "";
  // A signup's new key has not been stored yet. Keep its tab alive when a
  // browser blocks the wallet tab so the player can allow popups and retry.
  if (signup && !tab) {
    if (status) status.textContent = "Allow popups for this site, then select Set up Satchel again.";
    return;
  }
  try {
    const name = signup ? link.dataset.satchelName || null : await satchelName(signer);
    const signed = await signer.signHandoff(`${origin}/auth/nostr/handoff`, name);
    if (signup && tab.closed) throw new Error("The Satchel tab was closed");
    // Popup blocking must not leave a dead button. Same-tab navigation still
    // opens the checked invoice, and the entry can be resumed on return.
    postHandoff(origin, signed, next, tab && !tab.closed ? SATCHEL_TAB : "_self");
  } catch (error) {
    if (signup) {
      if (tab && !tab.closed) tab.close();
      if (status) status.textContent = "Could not connect your account to Satchel. Try again and approve the sign-in request.";
      return;
    }
    // The extension refused to sign, say: Satchel's own sign-in, then the same page.
    console.warn("Satchel sign-in failed; opening its sign-in page:", error);
    if (tab && !tab.closed) tab.location.href = link.href;
    else window.location.href = link.href;
  }
}

// The Lightning Address of the player's Satchel wallet, or null when they have none there.
async function satchelAddress(origin, signer) {
  const url = `${origin}/api/v1/address`;
  const response = await fetch(url, {
    headers: { Authorization: await signer.getAuthHeader(url, "GET", null) },
    credentials: "omit",
  });
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`Satchel answered ${response.status}`);
  const { lightning_address: address } = await response.json();
  return typeof address === "string" && address ? address : null;
}

// The Payouts page's line under the Lightning Address field (`satchel_address` in
// pages/payouts): a link to Satchel for an address, which becomes a button that fills
// the field in once Satchel says the player has one. Hidden when the field holds it already.
async function offerSatchelAddress(line) {
  const origin = satchelOrigin();
  const signer = satchelSigner();
  if (!line || line.dataset.checked || !origin || !signer) return;
  line.dataset.checked = "true";
  let address;
  try {
    address = await satchelAddress(origin, signer);
  } catch (error) {
    console.warn("Could not look up the Satchel Lightning Address:", error);
    return;
  }
  if (!address || !line.isConnected || satchelSigner() !== signer) return;
  const field = document.getElementById("payoutLightningAddress");
  if (field && field.value.trim().toLowerCase() === address.toLowerCase()) {
    line.classList.add("is-hidden");
    return;
  }
  const use = document.createElement("button");
  use.type = "button";
  use.className = "button is-text is-small";
  use.textContent = `Use ${address}`;
  use.addEventListener("click", () => {
    if (!field) return;
    field.value = address;
    field.focus();
  });
  line.replaceChildren(use);
}

function setupSatchel() {
  if (!satchelOrigin()) return;
  document.addEventListener("click", handOffToSatchel);
  // The Payouts page is swapped in by htmx, signed, once the player is logged in.
  document.addEventListener("htmx:after:process", () =>
    offerSatchelAddress(document.getElementById("satchelAddress")),
  );
  document.body.addEventListener("fw:logout", () => {
    satchelUsername = { signer: null, name: null };
  });
}
