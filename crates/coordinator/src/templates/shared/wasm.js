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
// The compiled module, kept so the log-in worker can instantiate it without
// fetching or compiling it again.
let wasmModule = null;

function wasmUrls() {
  const version = document.body.dataset.wasmVersion;
  const query = version ? `?v=${version}` : "";
  return {
    glue: `/ui/pkg/coordinator_wasm.js${query}`,
    module: `/ui/pkg/coordinator_wasm_bg.wasm${query}`,
  };
}

function initWasm() {
  if (!wasmLoading) {
    const urls = wasmUrls();
    wasmLoading = Promise.all([
      import(urls.glue),
      // Compiles while it downloads; the fallback covers a server that
      // doesn't send application/wasm.
      WebAssembly.compileStreaming(fetch(urls.module)).catch(() =>
        fetch(urls.module)
          .then((response) => response.arrayBuffer())
          .then((bytes) => WebAssembly.compile(bytes)),
      ),
    ])
      .then(async ([wasm, module]) => {
        await wasm.default({ module_or_path: module });
        wasmModule = module;
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

// The only script URL this page hands to a worker: the log-in worker's
// hashed asset URL, which the server stamps on the page. Trusted Types allow
// scripts only through named policies (see the CSP), so this policy admits
// that one URL and nothing else.
const loginWorkerPolicy = window.trustedTypes?.createPolicy("login-worker", {
  createScriptURL: (url) => {
    if (url !== document.body.dataset.loginWorker) {
      throw new TypeError(`not the log-in worker: ${url}`);
    }
    return url;
  },
});

// scrypt(password) in a worker, so the page stays responsive for the second
// or two it takes. Only the stretched bytes come back, and
// LoginCredentials.fromStretched zeroes them; the Nostr key is unsealed in
// this page's WASM as before. If the worker can't run (no Worker, or no
// dynamic import in workers), the page stretches the password itself.
function stretchInWorker(username, password) {
  const url = document.body.dataset.loginWorker;
  if (!url || !wasmModule || typeof Worker === "undefined") {
    return Promise.reject(new Error("no log-in worker"));
  }
  return new Promise((resolve, reject) => {
    const worker = new Worker(
      loginWorkerPolicy ? loginWorkerPolicy.createScriptURL(url) : url,
    );
    const done = (settle) => (value) => {
      worker.terminate();
      settle(value);
    };
    worker.onmessage = (event) =>
      event.data?.stretched instanceof Uint8Array
        ? done(resolve)(event.data.stretched)
        : done(reject)(new Error(event.data?.error || "log-in worker failed"));
    worker.onerror = (event) => {
      event.preventDefault?.();
      done(reject)(new Error(event.message || "log-in worker failed"));
    };
    worker.postMessage({
      module: wasmModule,
      glue: new URL(wasmUrls().glue, location.origin).href,
      username,
      password,
    });
  });
}

// LoginCredentials for a username and password, without blocking the page.
async function deriveLoginCredentials(username, password) {
  await initWasm();
  let stretched;
  try {
    stretched = await stretchInWorker(username, password);
  } catch (error) {
    console.warn("Stretching the password on the page:", error.message);
    return session.wasm.LoginCredentials.derive(username, password);
  }
  return session.wasm.LoginCredentials.fromStretched(stretched);
}

// Whether someone is logged in, with a signer and a wallet.
function isLoggedIn() {
  return Boolean(session.nostrClient?.isSignerReady?.() && session.dlcWallet);
}

// Loading the module hands out no key, so tests may call this.
window.initWasm = initWasm;
