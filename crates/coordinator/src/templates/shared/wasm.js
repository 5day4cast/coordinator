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
        window.NostrClientWrapper = wasm.NostrClientWrapper;
        window.DlcWallet = wasm.DlcWallet;
        window.LoginCredentials = wasm.LoginCredentials;
        window.SignerType = wasm.SignerType;
        window.nostrClient = new wasm.NostrClientWrapper();
        window.wasmInitialized = true;
      })
      .catch((error) => {
        // A later attempt, for example after a network error, starts over.
        wasmLoading = null;
        window.wasmError = error;
        console.error("WASM initialization failed:", error);
        throw error;
      });
  }
  return wasmLoading;
}

window.wasmInitialized = false;
window.initWasm = initWasm;
