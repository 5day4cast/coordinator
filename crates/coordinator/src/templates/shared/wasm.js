// The wallet session: the WASM classes once loaded, and the signer and wallet
// of whoever is logged in. build.rs wraps the bundle in one function, so this
// is shared by the bundle's scripts but not reachable from window: a script
// injected into the page could not pick up the signer and sign what it likes.
const session = {
  // NostrClientWrapper, DlcWallet, LoginCredentials and SignerType.
  wasm: null,
  // Signs for the logged-in account.
  nostrClient: null,
  // The logged-in account's wallet.
  dlcWallet: null,
};

// Loads the WASM wallet once. wasm-pack builds it separately into the UI
// directory; the server stamps its hash on the page so browsers can cache it.
let wasmLoading = null;

function initWasm() {
  if (!wasmLoading) {
    const version = document.body.dataset.wasmVersion;
    const query = version ? `?v=${version}` : "";
    wasmLoading = import(`/ui/pkg/coordinator_wasm.js${query}`)
      .then(async (wasm) => {
        await wasm.default({
          module_or_path: `/ui/pkg/coordinator_wasm_bg.wasm${query}`,
        });
        session.wasm = {
          NostrClientWrapper: wasm.NostrClientWrapper,
          DlcWallet: wasm.DlcWallet,
          LoginCredentials: wasm.LoginCredentials,
          SignerType: wasm.SignerType,
        };
        session.nostrClient = new wasm.NostrClientWrapper();
      })
      .catch((error) => {
        // A later attempt, for example after a network error, starts over.
        wasmLoading = null;
        console.error("WASM initialization failed:", error);
        throw error;
      });
  }
  return wasmLoading;
}

// Whether someone is logged in, with a signer and a wallet.
function isLoggedIn() {
  return Boolean(session.nostrClient?.isSignerReady?.() && session.dlcWallet);
}

// Loading the module hands out no key, so tests may call this.
window.initWasm = initWasm;
