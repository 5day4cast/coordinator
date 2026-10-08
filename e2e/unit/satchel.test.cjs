const assert = require("node:assert/strict");
const test = require("node:test");
const { loadBundle } = require("./bundle.cjs");

const SATCHEL = "https://wallet.example.org";
const INVOICE = "lntbs53000n1ticket";

// A page with Satchel configured (or not), and a player logged in with `signer` (or nobody).
// `answer` is Satchel's answer to the address lookup.
function page({ satchel = SATCHEL, signer = null, username = "alice", elements = {}, answer = null, blocked = false } = {}) {
  const listeners = {};
  const forms = [];
  const opened = [];
  const lookups = [];
  const fetched = [];
  const tab = { location: { href: "" }, close() { this.closed = true; } };
  const document = {
    body: {
      dataset: { apiBase: "https://5day4cast.com", ...(satchel ? { satchelUrl: satchel } : {}) },
      append: () => {},
      addEventListener: (type, listener) => (listeners[`body:${type}`] = listener),
    },
    getElementById: (id) => elements[id] ?? null,
    addEventListener: (type, listener) => (listeners[type] = listener),
    createElement: (tag) => {
      const element = {
        tag,
        children: [],
        listeners: {},
        append(...children) { this.children.push(...children); },
        addEventListener(type, listener) { this.listeners[type] = listener; },
        submit() { this.submitted = true; },
        remove() { this.removed = true; },
      };
      if (tag === "form") forms.push(element);
      return element;
    },
  };
  const window = {
    location: { href: "" },
    open: (...args) => {
      opened.push(args);
      return blocked ? null : tab;
    },
  };
  class AuthorizedClient {
    constructor(client, base) {
      assert.equal(client, signer, "signed by the logged-in player");
      assert.equal(base, "https://5day4cast.com");
    }
    async post(url) {
      lookups.push(url);
      return { ok: true, json: async () => ({ username }) };
    }
  }
  const fetch = async (url, options) => {
    fetched.push({ url, options });
    return answer;
  };
  const session = { nostrClient: signer };
  const satchel_js = loadBundle(["shared/satchel.js"],
    { document, window, session, AuthorizedClient, fetch, console: { warn: () => {} } },
    ["handOffToSatchel", "offerSatchelAddress", "setupSatchel"]);
  return { ...satchel_js, listeners, forms, opened, lookups, fetched, tab, session, window };
}

// A logged-in player's signer: their key in the page, or a Nostr extension.
function signer({ refuse = false } = {}) {
  const signed = [];
  return {
    signed,
    isSignerReady: () => true,
    signHandoff: async (url, name) => {
      if (refuse) throw new Error("the extension refused");
      signed.push({ url, name });
      return JSON.stringify({ kind: 27235, tags: [["u", url], ["method", "POST"], ["name", name]] });
    },
    getAuthHeader: async (url, method, body) => `Nostr signed ${method} ${url} ${body}`,
  };
}

function click(link) {
  return {
    target: { closest: (selector) => (selector === "a[data-satchel-next]" ? link : null) },
    preventDefault() { this.prevented = true; },
  };
}

function payLink() {
  return { dataset: { satchelNext: `/launch/lightning/${INVOICE}` }, href: `${SATCHEL}/launch/lightning/${INVOICE}` };
}

test("with the player's key, Pay with Satchel opens its tab in the click and posts the signed handoff there", async () => {
  const player = signer();
  const { handOffToSatchel, forms, opened, lookups } = page({ signer: player });
  const event = click(payLink());
  const done = handOffToSatchel(event);
  // In the click itself, before anything is signed: no popup blocker stops the tab.
  assert.ok(event.prevented);
  assert.deepEqual(opened, [["", "satchel"]]);
  await done;

  assert.deepEqual(lookups, ["https://5day4cast.com/api/v1/users/login"]);
  assert.deepEqual(player.signed, [{ url: `${SATCHEL}/auth/nostr/handoff`, name: "alice" }]);
  assert.equal(forms.length, 1);
  const [form] = forms;
  assert.equal(form.method, "post");
  assert.equal(form.action, `${SATCHEL}/auth/nostr/handoff`);
  assert.equal(form.target, "satchel");
  assert.ok(form.hidden && form.submitted && form.removed);
  const fields = Object.fromEntries(form.children.map((input) => [input.name, input]));
  assert.deepEqual(Object.keys(fields), ["event", "next"]);
  assert.ok(form.children.every((input) => input.type === "hidden"));
  assert.equal(JSON.parse(fields.event.value).kind, 27235);
  assert.equal(fields.next.value, `/launch/lightning/${INVOICE}`);

  // Open Satchel: the wallet, without asking for the username again.
  const open = { dataset: { satchelNext: "/wallet" }, href: `${SATCHEL}/wallet` };
  await handOffToSatchel(click(open));
  assert.equal(lookups.length, 1);
  assert.equal(forms[1].children.find((input) => input.name === "next").value, "/wallet");
});

test("an account without a username hands off without a name", async () => {
  const player = signer();
  const { handOffToSatchel } = page({ signer: player, username: null });
  await handOffToSatchel(click(payLink()));
  assert.deepEqual(player.signed, [{ url: `${SATCHEL}/auth/nostr/handoff`, name: null }]);
});

test("without the player's key, the link opens Satchel itself", async () => {
  for (const nostrClient of [null, { isSignerReady: () => false }]) {
    const { handOffToSatchel, forms, opened } = page({ signer: nostrClient });
    const event = click(payLink());
    await handOffToSatchel(event);
    assert.ok(!event.prevented, "the browser follows the link");
    assert.deepEqual(opened, []);
    assert.deepEqual(forms, []);
  }
});

test("a refused signature opens Satchel's sign-in in the tab instead", async () => {
  const { handOffToSatchel, forms, tab } = page({ signer: signer({ refuse: true }) });
  await handOffToSatchel(click(payLink()));
  assert.deepEqual(forms, []);
  assert.equal(tab.location.href, `${SATCHEL}/launch/lightning/${INVOICE}`);
});

test("a blocked popup still opens the invoice in the current browser tab", async () => {
  const { handOffToSatchel, forms } = page({ signer: signer(), blocked: true });
  await handOffToSatchel(click(payLink()));
  assert.equal(forms[0].target, "_self");
  assert.equal(forms[0].children.find(input => input.name === "next").value, `/launch/lightning/${INVOICE}`);

  const refused = page({ signer: signer({ refuse: true }), blocked: true });
  await refused.handOffToSatchel(click(payLink()));
  assert.equal(refused.window.location.href, payLink().href);
});

test("signup hands off its new key and suggested username without logging into a nonexistent account", async () => {
  const player = signer();
  const state = page({ signer: player });
  const link = { dataset: { satchelNext: "/wallet", signupSatchel: "", satchelName: "newplayer" }, href: `${SATCHEL}/wallet` };
  await state.handOffToSatchel(click(link));
  assert.deepEqual(state.lookups, []);
  assert.deepEqual(player.signed, [{ url: `${SATCHEL}/auth/nostr/handoff`, name: "newplayer" }]);
  assert.equal(state.forms[0].target, "satchel");
});

test("signup retains its unsaved key when popups or signing are refused", async () => {
  for (const blocked of [false, true]) {
    const status = { textContent: "" };
    const state = page({ signer: signer({ refuse: true }), blocked });
    const link = { dataset: { satchelNext: "/wallet", signupSatchel: "" }, href: `${SATCHEL}/wallet`,
      parentElement: { querySelector: () => status } };
    await state.handOffToSatchel(click(link));
    assert.deepEqual(state.forms, []);
    assert.equal(state.window.location.href, "");
    assert.equal(state.tab.location.href, "");
    assert.match(status.textContent, blocked ? /Allow popups/ : /Could not connect/);
  }
});

test("without Satchel configured, nothing is set up and links are left alone", async () => {
  const { handOffToSatchel, setupSatchel, listeners, opened } = page({ satchel: null, signer: signer() });
  setupSatchel();
  assert.deepEqual(Object.keys(listeners), []);
  const event = click(payLink());
  await handOffToSatchel(event);
  assert.ok(!event.prevented);
  assert.deepEqual(opened, []);
});

// Satchel's answer for a player with a wallet there.
const ALICE = { ok: true, status: 200,
  json: async () => ({ lightning_address: "alice@wallet.example.org", username: "alice" }) };

// The Payouts page's line under the Lightning Address field, and the field.
function payoutsLine(stored = "") {
  const classes = new Set();
  return {
    satchelAddress: {
      dataset: {},
      isConnected: true,
      classes,
      classList: { add: (name) => classes.add(name) },
      replaceChildren(...children) { this.children = children; },
    },
    payoutLightningAddress: { value: stored, focus() { this.focused = true; } },
  };
}

test("a player with a Satchel wallet gets a button that fills in its Lightning Address", async () => {
  const elements = payoutsLine("thor@lnurl.example");
  const { offerSatchelAddress, fetched } = page({ signer: signer(), elements, answer: ALICE });
  await offerSatchelAddress(elements.satchelAddress);

  // NIP-98 for exactly this GET, and no cookies.
  assert.equal(fetched.length, 1);
  assert.equal(fetched[0].url, `${SATCHEL}/api/v1/address`);
  // Objects from the bundle's context compare by their JSON.
  assert.deepEqual(JSON.parse(JSON.stringify(fetched[0].options)), {
    headers: { Authorization: `Nostr signed GET ${SATCHEL}/api/v1/address null` },
    credentials: "omit",
  });
  const [use] = elements.satchelAddress.children;
  assert.equal(use.tag, "button");
  assert.equal(use.type, "button");
  assert.equal(use.textContent, "Use alice@wallet.example.org");
  use.listeners.click();
  assert.equal(elements.payoutLightningAddress.value, "alice@wallet.example.org");
  assert.ok(elements.payoutLightningAddress.focused);

  // Asked once for the line.
  await offerSatchelAddress(elements.satchelAddress);
  assert.equal(fetched.length, 1);
});

test("a player without a Satchel wallet, or without a key, keeps the line that gets one", async () => {
  const without = payoutsLine();
  const answer = { ok: false, status: 404, json: async () => ({ error: "no_account" }) };
  const { offerSatchelAddress, fetched } = page({ signer: signer(), elements: without, answer });
  await offerSatchelAddress(without.satchelAddress);
  assert.equal(fetched.length, 1);
  assert.equal(without.satchelAddress.children, undefined);
  assert.ok(!without.satchelAddress.classes.has("is-hidden"));

  const signedOut = payoutsLine();
  const nobody = page({ elements: signedOut });
  await nobody.offerSatchelAddress(signedOut.satchelAddress);
  assert.deepEqual(nobody.fetched, []);
  assert.equal(signedOut.satchelAddress.children, undefined);
});

test("the line hides when the account already uses its Satchel address", async () => {
  const elements = payoutsLine("Alice@wallet.example.org ");
  const { offerSatchelAddress } = page({ signer: signer(), elements, answer: ALICE });
  await offerSatchelAddress(elements.satchelAddress);
  assert.ok(elements.satchelAddress.classes.has("is-hidden"));
  assert.equal(elements.satchelAddress.children, undefined);
});
