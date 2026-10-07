// The proof of work a new account carries (the server's domain/users/pow.rs).
// The sign-up dialog fetches a challenge as it opens, and a worker
// (static/pow-worker.js) solves it in the background, usually before the
// visitor has finished typing. A sign-up sent sooner waits for it, its button
// reading "Preparing…". A solution is good for one sign-up: each sign-up sent
// takes it, and the next one starts afresh. A server with proofs turned off
// answers the challenge request with 204, and sign-ups send none.

// The server keeps a challenge for ten minutes; one fetched longer ago than
// this is solved again rather than sent.
const SIGNUP_POW_FRESH_MS = 9 * 60 * 1000;

// The challenge being solved, or solved: { promise, fetchedAt, settled, worker }.
let signupPow = null;

function powWorkerUrl() {
  return document.getElementById("registerModal")?.dataset.powWorker;
}

// The only script URL the sign-up dialog hands to a worker: the proof-of-work
// worker's hashed asset URL, stamped on the dialog. Like the log-in worker's
// policy (shared/wasm.js), it admits that one URL and nothing else. Made on
// first use, so pages that never open the dialog never make it.
let powWorkerPolicy = null;

function powWorkerScript(url) {
  if (!window.trustedTypes) return url;
  powWorkerPolicy ??= window.trustedTypes.createPolicy("pow-worker", {
    createScriptURL: (candidate) => {
      if (candidate !== powWorkerUrl()) {
        throw new TypeError(`not the proof-of-work worker: ${candidate}`);
      }
      return candidate;
    },
  });
  return powWorkerPolicy.createScriptURL(url);
}

// Resolves to the nonce solving `challenge` at `difficulty` bits, in decimal.
function solvePowInWorker(attempt, challenge, difficulty) {
  const url = powWorkerUrl();
  if (!url || typeof Worker === "undefined") {
    return Promise.reject(new Error("no proof-of-work worker"));
  }
  return new Promise((resolve, reject) => {
    const worker = new Worker(powWorkerScript(url));
    attempt.worker = worker;
    const done = (settle) => (value) => {
      worker.terminate();
      attempt.worker = null;
      settle(value);
    };
    worker.onmessage = (event) =>
      typeof event.data?.nonce === "string"
        ? done(resolve)(event.data.nonce)
        : done(reject)(new Error(event.data?.error || "proof-of-work worker failed"));
    worker.onerror = (event) => {
      event.preventDefault?.();
      done(reject)(new Error(event.message || "proof-of-work worker failed"));
    };
    worker.postMessage({ challenge, difficulty });
  });
}

// The fields a sign-up sends: pow_challenge and pow_nonce, or none when the
// server asks for no proof.
async function fetchAndSolvePow(attempt) {
  const apiBase = document.body?.dataset.apiBase ?? "";
  const response = await fetch(`${apiBase}/api/v1/users/pow`, { method: "POST" });
  if (response.status === 204) return {};
  if (!response.ok) throw new Error(`proof-of-work challenge: HTTP ${response.status}`);
  const { challenge, difficulty } = await response.json();
  const nonce = await solvePowInWorker(attempt, challenge, difficulty);
  return { pow_challenge: challenge, pow_nonce: nonce };
}

// Starts fetching and solving a challenge, unless one is under way or solved
// and still fresh.
function startSignupPow() {
  if (signupPow && Date.now() - signupPow.fetchedAt < SIGNUP_POW_FRESH_MS) return;
  signupPow?.worker?.terminate();
  const attempt = { fetchedAt: Date.now(), settled: false, worker: null };
  attempt.promise = fetchAndSolvePow(attempt);
  attempt.promise.then(
    () => {
      attempt.settled = true;
    },
    (error) => {
      attempt.settled = true;
      console.warn("Sign-up proof of work failed:", error);
      // The next sign-up starts over.
      if (signupPow === attempt) signupPow = null;
    },
  );
  signupPow = attempt;
}

// Takes the proof for one sign-up, waiting for it if it is not solved yet;
// `button` reads "Preparing…" meanwhile.
async function takeSignupPow(button) {
  startSignupPow();
  const attempt = signupPow;
  signupPow = null;
  if (attempt.settled || !button) return attempt.promise;
  const label = button.textContent;
  const disabled = button.disabled;
  button.disabled = true;
  button.textContent = "Preparing…";
  try {
    return await attempt.promise;
  } finally {
    button.textContent = label;
    button.disabled = disabled;
  }
}

// Whether the server refused a request's proof of work.
async function powRefused(error) {
  try {
    return (await error.response?.clone().json())?.code === "pow_rejected";
  } catch (_) {
    return false;
  }
}

// Sends a sign-up: `send(proof)` posts it with the proof's fields. A proof
// the server refused (it restarted, or sign-ups got busy and need a harder
// one) is solved again and sent once more. After any failure the next proof
// starts solving, so a corrected sign-up is ready to go.
async function sendWithSignupPow(button, send) {
  for (let attempt = 0; ; attempt++) {
    const proof = await takeSignupPow(button);
    try {
      return await send(proof);
    } catch (error) {
      startSignupPow();
      if (attempt === 0 && (await powRefused(error))) continue;
      throw error;
    }
  }
}
