const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const source = readFileSync(
  path.join(__dirname, "../../crates/coordinator/src/templates/components/modals/modals.js"),
  "utf8",
);

for (const retried of [false, true]) {
  test(`username registration uses the stored wallet (${retried ? "retry" : "new account"})`, async () => {
    let createdWalletFreed = false;
    let loadedBackup;
    const candidateBackup = "new candidate wallet";
    const persistedBackup = retried ? "previously stored wallet" : candidateBackup;
    const restoredWallet = { persisted: true };
    const nostrClient = { getPublicKey: async () => "npub" };
    const register = (url, body) => {
      assert.ok(url.endsWith("/users/username/register"));
      assert.equal(body.encrypted_bitcoin_private_key, candidateBackup);
      return { ok: true };
    };
    const window = {
      nostrClient,
      AuthorizedClient: class {
        async post(url, body) {
          if (url.endsWith("/users/username/register")) return register(url, body);
          assert.ok(url.endsWith("/users/login"));
          return {
            ok: true,
            status: 201,
            json: async () => ({
              encrypted_bitcoin_private_key: persistedBackup,
              network: "signet",
            }),
          };
        }
      },
      DlcWallet: {
        create: () => ({
          encryptedBackup: async () => ({ encrypted_bitcoin_private_key: candidateBackup }),
          free: () => { createdWalletFreed = true; },
        }),
        load: async (signer, network, backup) => {
          assert.equal(signer, nostrClient);
          assert.equal(network, "signet");
          loadedBackup = backup;
          return restoredWallet;
        },
      },
    };
    const context = vm.createContext({
      window,
      console,
      document: { querySelector: () => null, getElementById: () => null },
      fetch: async (url, options) => register(url, JSON.parse(options.body)),
      setTimeout: () => {},
    });
    vm.runInContext(source, context);
    const manager = new window.AuthManager("https://coordinator.example", "signet");
    manager.pendingRegistration = { username: "alice", authKey: "credential", sealedNsec: "sealed" };

    await manager.handleUsernameRegisterStep2();

    assert.equal(loadedBackup, persistedBackup);
    assert.equal(window.dlcWallet, restoredWallet);
    assert.equal(createdWalletFreed, true);
    assert.equal(manager.pendingRegistration, null);
  });
}

test("password reset signs the replacement credentials with the recovered key", async () => {
  const endpoint = "https://coordinator.example/api/v1/users/username/reset-password";
  const newPassword = "Valid-password1";
  let signedBody;
  let sentBody;
  let credentialsFreed = false;
  const credentials = { authKey: "derived auth key", free: () => { credentialsFreed = true; } };
  const window = {
    nostrClient: {
      sealForLogin: (login) => {
        assert.equal(login, credentials);
        return "sealed nsec";
      },
      getAuthHeader: async (url, method, body) => {
        assert.equal(url, endpoint);
        assert.equal(method, "POST");
        signedBody = body;
        return "Nostr signed-payload";
      },
    },
    LoginCredentials: {
      derive: (username, password) => {
        assert.equal(username, "alice");
        assert.equal(password, newPassword);
        return credentials;
      },
    },
    NostrClientWrapper: class {},
    closeModal: () => {},
    openModal: () => {},
  };
  const context = vm.createContext({
    window,
    console,
    document: {
      querySelector: () => null,
      getElementById: (id) => ["forgotNewPassword", "forgotNewPasswordConfirm"].includes(id)
        ? { value: newPassword } : null,
    },
    fetch: async (url, options) => {
      assert.equal(url, endpoint);
      assert.equal(options.headers.Authorization, "Nostr signed-payload");
      sentBody = options.body;
      return { ok: true };
    },
  });
  vm.runInContext(readFileSync(
    path.join(__dirname, "../../crates/coordinator/src/templates/shared/authorized_client.js"),
    "utf8",
  ), context);
  vm.runInContext(source, context);
  const manager = new window.AuthManager("https://coordinator.example", "signet");
  manager.forgotUsername = "alice";
  manager.forgotChallenge = "challenge";
  manager.forgotSignedChallenge = "signed challenge";

  await manager.handleForgotStep3();

  assert.equal(signedBody, sentBody);
  assert.deepEqual(JSON.parse(sentBody), {
    username: "alice",
    challenge: "challenge",
    signed_event: "signed challenge",
    new_auth_key: "derived auth key",
    new_encrypted_nsec: "sealed nsec",
  });
  assert.equal(credentialsFreed, true);
  assert.equal(manager.forgotChallenge, null);
});
