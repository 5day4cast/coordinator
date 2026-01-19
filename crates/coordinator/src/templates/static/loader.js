// Loader: Initialize WASM, then load app bundle
import init, {
  NostrClientWrapper,
  TaprootWallet,
  TaprootWalletBuilder,
  SignerType,
  encryptNsecWithPassword,
  decryptNsecWithPassword,
  signForgotPasswordChallenge,
} from "/ui/pkg/coordinator_wasm.js";

// Expose WASM types globally
window.NostrClientWrapper = NostrClientWrapper;
window.TaprootWallet = TaprootWallet;
window.TaprootWalletBuilder = TaprootWalletBuilder;
window.SignerType = SignerType;

// Expose password crypto functions globally
window.encryptNsecWithPassword = encryptNsecWithPassword;
window.decryptNsecWithPassword = decryptNsecWithPassword;
window.signForgotPasswordChallenge = signForgotPasswordChallenge;

// Track WASM initialization state
window.wasmInitialized = false;
window.wasmError = null;

// Initialize WASM and create default client
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

// Load app bundle
const script = document.createElement("script");
script.src = "/ui/app.min.js";
document.head.appendChild(script);
