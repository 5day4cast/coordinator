// Loader: Initialize WASM, then load app bundle
import init, {
  NostrClientWrapper,
  TaprootWallet,
  TaprootWalletBuilder,
  SignerType,
} from "/ui/pkg/coordinator_wasm.js";

// Expose WASM types globally
window.NostrClientWrapper = NostrClientWrapper;
window.TaprootWallet = TaprootWallet;
window.TaprootWalletBuilder = TaprootWalletBuilder;
window.SignerType = SignerType;

// Initialize WASM and create default client
window.initWasm = async function () {
  await init();
  window.nostrClient = new NostrClientWrapper();
};

// Load app bundle
const script = document.createElement("script");
script.src = "/ui/app.min.js";
document.head.appendChild(script);
