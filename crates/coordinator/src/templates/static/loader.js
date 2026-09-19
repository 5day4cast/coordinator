import init, {
  NostrClientWrapper,
  DlcWallet,
  LoginCredentials,
  SignerType,
} from "/ui/pkg/coordinator_wasm.js";

window.NostrClientWrapper = NostrClientWrapper;
window.DlcWallet = DlcWallet;
window.LoginCredentials = LoginCredentials;
window.SignerType = SignerType;

window.wasmInitialized = false;
window.wasmError = null;

window.initWasm = async function () {
  try {
    await init();
    window.nostrClient = new NostrClientWrapper();
    window.wasmInitialized = true;
    console.log("WASM initialized successfully");
  } catch (error) {
    window.wasmError = error;
    console.error("WASM initialization failed:", error);
    throw error;
  }
};

const script = document.createElement("script");
script.src = "/ui/app.min.js";
document.head.appendChild(script);
