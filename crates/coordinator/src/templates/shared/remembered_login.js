// Keeps a login across reloads and tabs until the player logs out or stays
// away for REMEMBER_FOR_MS.
//
// - A username login stores the Nostr key encrypted with an AES-GCM key that
//   WebCrypto generated as non-extractable. Both live in IndexedDB: the raw
//   key is never written to disk, and the wrapping key cannot be read out of
//   the browser, only used by this origin.
// - An extension login stores only that the extension signs; the extension
//   keeps the key.
// - Logging out deletes the record, and every open tab logs out with it.

const REMEMBER_FOR_MS = 30 * 24 * 60 * 60 * 1000;
const REMEMBER_DB = "fw-session";
const REMEMBER_STORE = "login";
const REMEMBER_RECORD = "current";

// Tells this site's other tabs about logins and logouts.
const sessionChannel =
  typeof BroadcastChannel === "undefined" ? null : new BroadcastChannel("fw-session");

// Set while a page restores a remembered login, so account requests wait for
// it instead of asking for a password.
let loginRestoring = null;

function openRememberDb() {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(REMEMBER_DB, 1);
    request.onupgradeneeded = () => request.result.createObjectStore(REMEMBER_STORE);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function rememberStore(mode, action) {
  const db = await openRememberDb();
  try {
    return await new Promise((resolve, reject) => {
      const transaction = db.transaction(REMEMBER_STORE, mode);
      const request = action(transaction.objectStore(REMEMBER_STORE));
      transaction.oncomplete = () => resolve(request.result);
      transaction.onerror = () => reject(transaction.error);
      transaction.onabort = () => reject(transaction.error);
    });
  } finally {
    db.close();
  }
}

// Remembers the logged-in signer: `nsec` for a local key, null for an extension.
async function rememberLogin(nsec) {
  const record = { expiresAt: Date.now() + REMEMBER_FOR_MS, kind: "extension" };
  if (nsec) {
    const key = await crypto.subtle.generateKey({ name: "AES-GCM", length: 256 }, false, [
      "encrypt",
      "decrypt",
    ]);
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const sealed = await crypto.subtle.encrypt(
      { name: "AES-GCM", iv },
      key,
      new TextEncoder().encode(nsec),
    );
    Object.assign(record, { kind: "key", key, iv, sealed });
  }
  await rememberStore("readwrite", (store) => store.put(record, REMEMBER_RECORD));
}

// The remembered signer, `{ kind, nsec }`, or null. Using it extends it.
async function recallLogin() {
  const record = await rememberStore("readonly", (store) => store.get(REMEMBER_RECORD));
  if (!record) return null;
  if (record.expiresAt < Date.now()) {
    await forgetLogin();
    return null;
  }
  record.expiresAt = Date.now() + REMEMBER_FOR_MS;
  await rememberStore("readwrite", (store) => store.put(record, REMEMBER_RECORD));
  if (record.kind !== "key") return { kind: record.kind, nsec: null };
  const plain = await crypto.subtle.decrypt(
    { name: "AES-GCM", iv: record.iv },
    record.key,
    record.sealed,
  );
  return { kind: "key", nsec: new TextDecoder().decode(plain) };
}

async function forgetLogin() {
  await rememberStore("readwrite", (store) => store.delete(REMEMBER_RECORD));
}

// An extension injects window.nostr as the page loads, maybe after this runs.
async function nostrExtensionReady(waitMs = 3000) {
  const started = Date.now();
  while (!window.nostr && Date.now() - started < waitMs) {
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  return Boolean(window.nostr);
}
