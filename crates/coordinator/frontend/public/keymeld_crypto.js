/**
 * Keymeld Crypto Utilities
 *
 * Provides cryptographic functions for keymeld server-side registration.
 * These functions use the keymeld SDK via WASM (coordinator-wasm crate).
 */

/**
 * Derive the keymeld auth pubkey from the user's ephemeral private key and session ID.
 * Uses the keymeld SDK's derive_session_auth_pubkey function via WASM.
 *
 * @param {string} privateKeyHex - User's ephemeral private key (hex)
 * @param {string} sessionId - Keymeld session ID
 * @returns {Promise<string>} Derived auth public key (hex)
 */
export async function deriveKeymeldAuthPubkey(privateKeyHex, sessionId) {
  // Use the WASM function from coordinator-wasm
  if (typeof window.derive_keymeld_auth_pubkey === "function") {
    return window.derive_keymeld_auth_pubkey(privateKeyHex, sessionId);
  }

  // Fallback error if WASM not loaded
  throw new Error(
    "Keymeld WASM not loaded - derive_keymeld_auth_pubkey function not available",
  );
}

/**
 * Encrypt the user's ephemeral private key to the enclave's public key.
 * Uses the keymeld SDK's encrypt_private_key_for_enclave function via WASM.
 *
 * @param {string} privateKeyHex - User's ephemeral private key (hex)
 * @param {string} enclavePubkeyHex - Enclave's public key (hex)
 * @returns {Promise<string>} Encrypted private key (hex)
 */
export async function encryptToEnclave(privateKeyHex, enclavePubkeyHex) {
  // Use the WASM function from coordinator-wasm
  if (typeof window.encrypt_private_key_for_enclave === "function") {
    return window.encrypt_private_key_for_enclave(
      privateKeyHex,
      enclavePubkeyHex,
    );
  }

  // Fallback error if WASM not loaded
  throw new Error(
    "Keymeld WASM not loaded - encrypt_private_key_for_enclave function not available",
  );
}
