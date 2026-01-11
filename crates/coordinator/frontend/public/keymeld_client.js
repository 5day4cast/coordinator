/**
 * Simple Keymeld client for browser-based session joining
 *
 * This module provides a simple interface for users to join keymeld
 * keygen sessions after paying their HODL invoice. The user's browser
 * registers directly with keymeld using the WASM SDK.
 */

/**
 * Join a keymeld keygen session
 *
 * @param {Object} keymeldInfo - Keymeld session info from the coordinator
 * @param {string} keymeldInfo.gateway_url - Keymeld gateway URL
 * @param {string} keymeldInfo.session_id - Session ID to join
 * @param {string} keymeldInfo.encrypted_session_secret - NIP-44 encrypted session secret
 * @param {string} keymeldInfo.user_id - User's ID for the session
 * @param {string} ephemeralPrivateKey - User's ephemeral private key (hex)
 * @param {Object} nostrClient - Nostr client for decryption
 * @returns {Promise<Object>} Keygen result with session_id, session_secret, aggregate_key
 */
export async function joinKeymeldSession(keymeldInfo, ephemeralPrivateKey, nostrClient) {
  console.log("Joining keymeld session:", keymeldInfo.session_id);

  // Decrypt the session secret using NIP-44
  const sessionSecretHex = await nostrClient.nip44Decrypt(
    keymeldInfo.encrypted_session_secret
  );

  // Create keymeld participant using WASM
  const config = new window.KeymeldClientConfig(keymeldInfo.gateway_url);
  const participant = new window.KeymeldParticipant(
    config,
    keymeldInfo.user_id,
    ephemeralPrivateKey
  );

  console.log("Keymeld participant created, user_id:", participant.user_id);

  // Join the keygen session - this registers the user with keymeld
  const result = await participant.join_keygen_session(
    keymeldInfo.session_id,
    sessionSecretHex
  );

  console.log("Successfully joined keymeld session:", result.session_id);

  return result;
}

/**
 * Poll for keymeld info from the contract endpoint
 *
 * Keymeld info is only available after the user's HODL invoice is accepted.
 * This function polls until the info is available.
 *
 * @param {Object} client - Authorized API client
 * @param {string} competitionId - Competition ID
 * @param {number} maxAttempts - Maximum polling attempts (default: 30)
 * @param {number} delayMs - Delay between attempts in ms (default: 2000)
 * @returns {Promise<Object|null>} Keymeld info or null if not available
 */
export async function pollForKeymeldInfo(client, competitionId, maxAttempts = 30, delayMs = 2000) {
  console.log("Polling for keymeld info for competition:", competitionId);

  for (let attempt = 0; attempt < maxAttempts; attempt++) {
    try {
      const response = await client.get(
        `${client.apiBase}/api/v1/competitions/${competitionId}/contract`
      );

      if (response.ok) {
        const contract = await response.json();

        if (contract.keymeld && contract.keymeld.enabled) {
          console.log("Keymeld info available:", contract.keymeld.session_id);
          return contract.keymeld;
        }
      }

      // Not ready yet, wait and retry
      await new Promise((resolve) => setTimeout(resolve, delayMs));
    } catch (error) {
      console.warn("Error polling for keymeld info:", error);
      await new Promise((resolve) => setTimeout(resolve, delayMs));
    }
  }

  console.warn("Keymeld info not available after", maxAttempts, "attempts");
  return null;
}

/**
 * Complete keymeld registration flow after payment
 *
 * This is the main entry point for the simplified keymeld flow:
 * 1. Poll for keymeld info (available after payment)
 * 2. Join the keygen session
 * 3. Return success
 *
 * @param {Object} client - Authorized API client
 * @param {string} competitionId - Competition ID
 * @param {string} ephemeralPrivateKey - User's ephemeral private key (hex)
 * @param {Object} nostrClient - Nostr client for decryption
 * @returns {Promise<Object>} Result with success status and session info
 */
export async function completeKeymeldRegistration(
  client,
  competitionId,
  ephemeralPrivateKey,
  nostrClient
) {
  try {
    // Poll for keymeld info
    const keymeldInfo = await pollForKeymeldInfo(client, competitionId);

    if (!keymeldInfo) {
      return {
        success: false,
        error: "Keymeld info not available - payment may not have been received",
      };
    }

    // Join the keygen session
    const result = await joinKeymeldSession(keymeldInfo, ephemeralPrivateKey, nostrClient);

    return {
      success: true,
      sessionId: result.session_id,
      aggregateKey: result.aggregate_key,
    };
  } catch (error) {
    console.error("Failed to complete keymeld registration:", error);
    return {
      success: false,
      error: error.message || "Unknown error",
    };
  }
}
