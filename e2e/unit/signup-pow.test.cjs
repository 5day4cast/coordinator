// The sign-up proof of work in the browser: the worker's SHA-256 and solver
// (static/sha256.js, static/pow-worker.js) and the dialog's side
// (components/modals/signup_pow.js), against the scheme in the server's
// domain/users/pow.rs.
const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const test = require("node:test");
const { loadBundle } = require("./bundle.cjs");

// The vector the server's tests and pow-worker.js note.
const VECTOR = "AAECAwQFBgcICQoLDA0ODwAAAABqxpqYEAMhk91h_us6MSi5qOUB7I8";

function worker(self = {}) {
  return loadBundle(["static/sha256.js", "static/pow-worker.js"], { self, atob }, [
    "sha256",
    "solvePow",
    "decodeBase64Url",
    "hasLeadingZeros",
  ]);
}

const hex = (bytes) => Buffer.from(bytes).toString("hex");
// Objects made inside the bundle's context, as plain objects of this one.
const plain = (value) => JSON.parse(JSON.stringify(value));

test("the worker's SHA-256 matches Node's", () => {
  const { sha256 } = worker();
  assert.equal(
    hex(sha256(new TextEncoder().encode("abc"))),
    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
  );
  assert.equal(
    hex(sha256(new Uint8Array(0))),
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
  );
  for (let length = 0; length <= 200; length++) {
    const bytes = crypto.randomBytes(length);
    assert.equal(
      hex(sha256(new Uint8Array(bytes))),
      crypto.createHash("sha256").update(bytes).digest("hex"),
      `${length} bytes`,
    );
  }
});

test("the worker solves the shared test vector", () => {
  const { decodeBase64Url, solvePow } = worker();
  const challenge = decodeBase64Url(VECTOR);
  assert.equal(
    hex(challenge),
    "000102030405060708090a0b0c0d0e0f000000006ac69a9810032193dd61feeb3a3128b9a8e501ec8f",
  );
  assert.equal(solvePow(challenge, 16), "91039");
  assert.equal(solvePow(challenge, 0), "0");
});

test("the worker answers the page once, then closes", () => {
  const messages = [];
  let closed = 0;
  const self = { postMessage: (message) => messages.push(message), close: () => { closed += 1; } };
  worker(self);
  self.onmessage({ data: { challenge: VECTOR, difficulty: 16 } });
  self.onmessage({ data: { challenge: VECTOR, difficulty: 65 } });
  self.onmessage({ data: { challenge: VECTOR, difficulty: "16" } });
  assert.deepEqual(plain(messages[0]), { nonce: "91039" });
  assert.match(messages[1].error, /unexpected difficulty 65/);
  assert.match(messages[2].error, /unexpected difficulty 16/);
  assert.equal(closed, 3);
});

test("solutions check out with Node's crypto whatever the challenge's length", () => {
  const { solvePow, hasLeadingZeros } = worker();
  for (const length of [0, 1, 41, 55, 56, 57, 63, 64, 65, 120]) {
    const challenge = crypto.randomBytes(length);
    const nonce = BigInt(solvePow(new Uint8Array(challenge), 10));
    const suffix = Buffer.alloc(8);
    suffix.writeBigUInt64BE(nonce);
    const hash = crypto.createHash("sha256").update(Buffer.concat([challenge, suffix])).digest();
    assert.equal(hash[0], 0, `${length} bytes`);
    assert.equal(hash[1] >> 6, 0, `${length} bytes`);
  }
  const words = (...values) => Int32Array.from(values);
  assert.equal(hasLeadingZeros(words(0, 0x0000ffff), 48), true);
  assert.equal(hasLeadingZeros(words(0, 0x0000ffff), 49), false);
  assert.equal(hasLeadingZeros(words(0x00010000), 15), true);
  assert.equal(hasLeadingZeros(words(0x00010000), 16), false);
  assert.equal(hasLeadingZeros(words(-1), 0), true);
});

// The dialog's side, with a stand-in worker and server.
function dialog({ status = 200 } = {}) {
  const page = { fetches: 0, workers: [], policies: [], now: 0 };
  class Worker {
    constructor(url) {
      this.url = url;
      page.workers.push(this);
    }
    postMessage(message) {
      this.sent = message;
    }
    terminate() {
      this.terminated = true;
    }
  }
  const document = {
    body: { dataset: { apiBase: "https://coordinator.example" } },
    getElementById: (id) =>
      id === "registerModal" ? { dataset: { powWorker: "/assets/pow-worker.0123.js" } } : null,
  };
  const window = {
    trustedTypes: {
      createPolicy: (name, rules) => {
        page.policies.push(name);
        return { createScriptURL: (url) => ({ trusted: rules.createScriptURL(url) }) };
      },
    },
  };
  const fetch = async (url, options) => {
    assert.equal(url, "https://coordinator.example/api/v1/users/pow");
    assert.equal(options.method, "POST");
    page.fetches += 1;
    const challenge = `challenge ${page.fetches}`;
    return {
      status,
      ok: status < 300,
      json: async () => ({ challenge, difficulty: 3, expires_at: 1 }),
    };
  };
  const exported = loadBundle(["components/modals/signup_pow.js"], {
    window,
    document,
    fetch,
    Worker,
    console: { warn: () => {} },
    Date: { now: () => page.now },
  }, ["startSignupPow", "takeSignupPow", "sendWithSignupPow"]);
  return { ...exported, page };
}

const settle = () => new Promise((resolve) => setImmediate(resolve));

function button(label) {
  const shown = [];
  return {
    shown,
    disabled: false,
    get textContent() {
      return shown.at(-1) ?? label;
    },
    set textContent(text) {
      shown.push(text);
    },
  };
}

test("a sign-up waits for its proof only while it is unsolved, and spends it", async () => {
  const { startSignupPow, takeSignupPow, page } = dialog();
  startSignupPow();
  startSignupPow();
  await settle();
  assert.equal(page.fetches, 1, "one challenge at a time");
  assert.equal(page.workers.length, 1);
  const [first] = page.workers;
  assert.deepEqual(plain(first.url), { trusted: "/assets/pow-worker.0123.js" });
  assert.deepEqual(page.policies, ["pow-worker"]);
  assert.deepEqual(plain(first.sent), { challenge: "challenge 1", difficulty: 3 });

  // Sent before it is solved: the button says so until it is.
  const complete = button("Complete Registration");
  const taken = takeSignupPow(complete);
  await settle();
  assert.equal(complete.textContent, "Preparing…");
  assert.equal(complete.disabled, true);
  first.onmessage({ data: { nonce: "5" } });
  assert.deepEqual(plain(await taken), { pow_challenge: "challenge 1", pow_nonce: "5" });
  assert.equal(complete.textContent, "Complete Registration");
  assert.equal(complete.disabled, false);
  assert.equal(first.terminated, true);

  // Each proof is spent by one sign-up; one solved in time costs no wait.
  startSignupPow();
  await settle();
  assert.equal(page.fetches, 2);
  page.workers[1].onmessage({ data: { nonce: "9" } });
  await settle();
  const quick = button("Register with Extension");
  assert.deepEqual(plain(await takeSignupPow(quick)), { pow_challenge: "challenge 2", pow_nonce: "9" });
  assert.deepEqual(quick.shown, []);

  // A proof left unsent too long is solved again.
  startSignupPow();
  await settle();
  page.workers[2].onmessage({ data: { nonce: "1" } });
  page.now += 9 * 60 * 1000;
  startSignupPow();
  await settle();
  assert.equal(page.fetches, 4);
});

test("a server with proofs off asks for none", async () => {
  const { takeSignupPow, page } = dialog({ status: 204 });
  assert.deepEqual(plain(await takeSignupPow(null)), {});
  assert.equal(page.workers.length, 0);
});

test("a refused proof is solved again and sent once more", async () => {
  const { sendWithSignupPow, page } = dialog();
  const refused = (code) => {
    const error = new Error("HTTP error! status: 400");
    error.response = { clone: () => ({ json: async () => ({ error: "refused", code }) }) };
    return error;
  };
  // Answer every worker as soon as it starts.
  const answer = setInterval(() => {
    for (const worker of page.workers) {
      if (!worker.answered && worker.onmessage) {
        worker.answered = true;
        worker.onmessage({ data: { nonce: String(page.workers.indexOf(worker)) } });
      }
    }
  }, 1);
  try {
    const sent = [];
    const response = await sendWithSignupPow(null, async (proof) => {
      sent.push(proof);
      if (sent.length === 1) throw refused("pow_rejected");
      return "created";
    });
    assert.equal(response, "created");
    assert.deepEqual(plain(sent), [
      { pow_challenge: "challenge 1", pow_nonce: "0" },
      { pow_challenge: "challenge 2", pow_nonce: "1" },
    ]);

    // Refused twice, or for another reason, the error reaches the dialog, and
    // the next proof is already on its way.
    let tries = 0;
    await assert.rejects(
      sendWithSignupPow(null, async () => {
        tries += 1;
        throw refused("pow_rejected");
      }),
      /status: 400/,
    );
    assert.equal(tries, 2);
    await assert.rejects(
      sendWithSignupPow(null, async () => {
        throw refused(undefined);
      }),
      /status: 400/,
    );
    await settle();
    assert.equal(page.fetches, 6);
  } finally {
    clearInterval(answer);
  }
});
