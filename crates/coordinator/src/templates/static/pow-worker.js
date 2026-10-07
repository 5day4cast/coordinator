// Solves the sign-up proof of work off the page's thread (see
// components/modals/signup_pow.js, and the server's domain/users/pow.rs for
// the scheme). build.rs bundles it after sha256.js.
//
// The page sends { challenge, difficulty }: the challenge in base64url and
// the leading zero bits wanted. This worker finds the first u64 nonce such
// that SHA-256(challenge bytes ‖ nonce as 8 big-endian bytes) starts with
// that many zero bits, answers { nonce } in decimal (or { error }), then
// closes.
//
// Test vector, the same as the server's: the challenge
// AAECAwQFBgcICQoLDA0ODwAAAABqxpqYEAMhk91h_us6MSi5qOUB7I8 at 16 bits is first
// solved by nonce 91039 (SHA-256 0000ef4d…).

function decodeBase64Url(text) {
  const base64 = text.replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(base64 + "=".repeat((4 - (base64.length % 4)) % 4));
  return Uint8Array.from(binary, (char) => char.charCodeAt(0));
}

// Whether `state`, a SHA-256 as eight 32-bit words, starts with `bits` zero bits.
function hasLeadingZeros(state, bits) {
  for (let i = 0; bits > 0; i++, bits -= 32) {
    const word = state[i] >>> 0;
    if (bits < 32) return word >>> (32 - bits) === 0;
    if (word !== 0) return false;
  }
  return true;
}

// The first nonce solving `challenge` (bytes) at `difficulty` bits, in decimal.
function solvePow(challenge, difficulty) {
  const at = challenge.length;
  const { padded, view } = sha256Padded(new Uint8Array(at + 8));
  padded.set(challenge);
  const words = new Int32Array(padded.length >> 2);
  for (let i = 0; i < words.length; i++) words[i] = view.getInt32(i * 4);
  // Only the words holding the nonce change from one try to the next, and
  // only the blocks from the one it starts in need hashing again.
  const firstWord = at >> 2;
  const lastWord = (at + 7) >> 2;
  const firstBlock = at >> 6;
  const midstate = SHA256_IV.slice();
  const w = new Int32Array(64);
  for (let block = 0; block < firstBlock; block++) sha256Block(midstate, words, block * 16, w);
  const state = new Int32Array(8);
  for (let high = 0; high <= 0xffffffff; high++) {
    view.setUint32(at, high);
    for (let low = 0; low <= 0xffffffff; low++) {
      view.setUint32(at + 4, low);
      for (let i = firstWord; i <= lastWord; i++) words[i] = view.getInt32(i * 4);
      state.set(midstate);
      for (let offset = firstBlock * 16; offset < words.length; offset += 16) {
        sha256Block(state, words, offset, w);
      }
      if (hasLeadingZeros(state, difficulty)) return String(high * 0x100000000 + low);
    }
  }
  throw new Error("no nonce solves the challenge");
}

self.onmessage = (event) => {
  try {
    const { challenge, difficulty } = event.data;
    if (!Number.isInteger(difficulty) || difficulty < 0 || difficulty > 64) {
      throw new Error(`unexpected difficulty ${difficulty}`);
    }
    self.postMessage({ nonce: solvePow(decodeBase64Url(challenge), difficulty) });
  } catch (error) {
    self.postMessage({ error: String(error?.message || error) });
  } finally {
    self.close();
  }
};
