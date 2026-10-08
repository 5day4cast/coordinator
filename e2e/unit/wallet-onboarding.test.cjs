const assert = require("node:assert/strict");
const test = require("node:test");
const { loadBundle } = require("./bundle.cjs");

function element(value = "", hidden = false) {
  const classes = new Set(hidden ? ["is-hidden"] : []);
  return { value, textContent: "", dataset: {},
    classList: {
      add: name => classes.add(name), remove: name => classes.delete(name),
      contains: name => classes.has(name),
      toggle: (name, on) => on ? classes.add(name) : classes.delete(name),
    },
  };
}

function signup({ address = "alice@wallet.example.org", source = "satchel", fails = false } = {}) {
  const elements = Object.fromEntries([
    "usernameRegisterError", "usernameRegisterStep2Error", "extensionRegisterError",
    "registerLightningAddress", "extensionLightningAddress", "usernameNsecDisplay",
    "usernameRegisterStep1", "usernameRegisterStep2", "usernameRegisterStep3",
    "extensionRegisterButton",
  ].map(id => [id, element()]));
  Object.assign(elements, {
    registerUsernameInput: element("alice"),
    registerPassword: element("Password123!"), registerPasswordConfirm: element("Password123!"),
    registerLightningAddressSource: element(source), extensionLightningAddressSource: element(source),
    usernameSatchelSetup: { ...element("", true), querySelector: () => link },
    extensionSatchelSetup: element("", true),
  });
  const link = { dataset: {} };
  const calls = [];
  const lookups = [];
  let key = "";
  const client = {
    initialize(type) { key = type === "private" ? "new-private-key" : "extension-key"; },
    getPublicKey: async () => key,
    recoveryKey: () => "nsec-for-new-private-key",
    sealForLogin: () => "sealed-new-private-key",
    getAuthHeader: async () => `Signed by ${key}`,
  };
  const session = { nostrClient: client, wasm: {
    SignerType: { PrivateKey: "private", NIP07: "extension" },
    DlcWallet: { create: () => ({
      encryptedBackup: async () => ({ encrypted_bitcoin_private_key: "backup" }), free() {},
    }) },
  } };
  const { AuthManager } = loadBundle(["shared/satchel.js", "components/modals/modals.js"], {
    document: { body: { dataset: { satchelUrl: "https://wallet.example.org" } },
      getElementById: id => elements[id] ?? null,
      querySelector: selector => elements[selector.slice(1)] ?? null },
    session, window: {}, console: { error() {}, warn() {} }, setTimeout() {},
    deriveLoginCredentials: async () => ({ authKey: "login-auth", free() {} }),
    sendWithSignupPow: async (_, send) => send({}),
    AuthorizedClient: class { async post(url, body) { calls.push({ url, body, key }); return { ok: true }; } },
    fetch: async (url, options) => {
      lookups.push({ url, authorization: options.headers.Authorization });
      if (fails) throw new Error("offline");
      return { status: address ? 200 : 404, ok: true, json: async () => ({ lightning_address: address }) };
    },
  }, ["AuthManager"]);
  const manager = new AuthManager("https://game.example.org", "signet");
  manager.walletReady = async () => true;
  manager.loadWallet = async () => {};
  manager.performLogin = async () => {};
  manager.performRegistration = async lightningAddress => calls.push({ lightningAddress, key });
  return { manager, elements, calls, lookups, link, client };
}

test("Satchel signup prepares a recovery key before wallet setup and registers with that wallet's address", async () => {
  const { manager, elements, calls, lookups, link } = signup();
  await manager.handleUsernameRegisterStep1();
  assert.equal(elements.usernameRegisterError.textContent, "");
  assert.equal(elements.usernameNsecDisplay.value, "nsec-for-new-private-key");
  assert.equal(link.dataset.satchelName, "alice");
  assert.ok(!elements.usernameSatchelSetup.classList.contains("is-hidden"));
  assert.equal(calls.length, 0, "no registration until the wallet exists");
  await manager.handleUsernameRegisterStep2();
  assert.equal(elements.usernameRegisterStep2Error.textContent, "");
  assert.deepEqual(lookups, [{ url: "https://wallet.example.org/api/v1/address", authorization: "Signed by new-private-key" }]);
  assert.equal(calls[0].body.lightning_address, "alice@wallet.example.org");
  assert.equal(calls[0].key, "new-private-key");
  assert.equal(calls[0].body.encrypted_nsec, "sealed-new-private-key");
  assert.equal(manager.pendingRegistration, null);
});

test("an unfinished wallet or failed address lookup preserves signup for a retry", async () => {
  for (const options of [{ address: null }, { fails: true }, { address: "lnurl1invalid" }]) {
    const { manager, elements, calls } = signup(options);
    await manager.handleUsernameRegisterStep1();
    const prepared = manager.pendingRegistration;
    await manager.handleUsernameRegisterStep2();
    assert.equal(calls.length, 0);
    assert.equal(manager.pendingRegistration, prepared);
    assert.equal(elements.usernameNsecDisplay.value, "nsec-for-new-private-key");
    assert.match(elements.usernameRegisterStep2Error.textContent, /Satchel/);
  }
});

test("an existing address remains required, and that signup does not contact Satchel", async () => {
  const { manager, elements, calls, lookups } = signup({ source: "address" });
  await manager.handleUsernameRegisterStep1();
  assert.match(elements.usernameRegisterError.textContent, /Please enter the Lightning Address/);
  elements.registerLightningAddress.value = " Player@another.example ";
  await manager.handleUsernameRegisterStep1();
  assert.ok(elements.usernameSatchelSetup.classList.contains("is-hidden"));
  await manager.handleUsernameRegisterStep2();
  assert.equal(calls[0].body.lightning_address, "player@another.example");
  assert.deepEqual(lookups, []);
});

test("changing the signer cannot register the prepared wallet under a different key", async () => {
  const { manager, client, calls, elements } = signup();
  await manager.handleUsernameRegisterStep1();
  client.initialize("extension");
  await manager.handleUsernameRegisterStep2();
  assert.equal(calls.length, 0);
  assert.match(elements.usernameRegisterStep2Error.textContent, /key changed/);
});

test("extension signup connects first, then finishes with its Satchel address", async () => {
  const { manager, elements, calls, lookups } = signup();
  await manager.handleExtensionRegistration();
  assert.equal(calls.length, 0);
  assert.equal(lookups.length, 0);
  assert.ok(!elements.extensionSatchelSetup.classList.contains("is-hidden"));
  assert.equal(elements.extensionRegisterButton.textContent, "Finish signup");
  await manager.handleExtensionRegistration();
  assert.equal(elements.extensionRegisterError.textContent, "");
  assert.deepEqual(calls, [{ lightningAddress: "alice@wallet.example.org", key: "extension-key" }]);
  assert.equal(lookups[0].authorization, "Signed by extension-key");
});
