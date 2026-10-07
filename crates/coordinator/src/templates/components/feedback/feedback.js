// The feedback form (components/feedback/mod.rs), in the footer's dialog or on
// /feedback. Like a sign-up it carries a proof of work, from its own
// challenge (GET /api/v1/feedback/challenge), solved by the same worker
// (signup_pow.js). Each form starts solving as it appears or is first
// focused, about a second while the visitor types; until then its Send button
// reads "Preparing…". If no proof can be made the message goes without one;
// the server takes fewer of those. htmx then sends the form as it is.
//
// The server fills in the page, the request and session ids and the signed-in
// key itself; the form only adds the proof. A refused message comes back as a
// new form, which gets a new proof.

async function fetchAndSolveFeedbackPow(attempt) {
  const apiBase = document.body?.dataset.apiBase ?? "";
  const response = await fetch(`${apiBase}/api/v1/feedback/challenge`);
  if (!response.ok) throw new Error(`feedback challenge: HTTP ${response.status}`);
  const { challenge, difficulty } = await response.json();
  const nonce = await solvePowInWorker(attempt, challenge, difficulty);
  return { challenge, nonce };
}

// Fills `form`'s proof fields once, holding its Send button meanwhile.
function prepareFeedbackForm(form) {
  if (!form || form.dataset.pow) return;
  form.dataset.pow = "solving";
  const button = form.querySelector('button[type="submit"]');
  const label = button?.textContent;
  if (button) {
    button.disabled = true;
    button.textContent = "Preparing…";
  }
  const attempt = { worker: null };
  fetchAndSolveFeedbackPow(attempt)
    .then(
      ({ challenge, nonce }) => {
        form.elements.namedItem("pow_challenge").value = challenge;
        form.elements.namedItem("pow_nonce").value = nonce;
      },
      (error) => console.warn("Feedback proof of work failed:", error),
    )
    .finally(() => {
      form.dataset.pow = "ready";
      if (button) {
        button.textContent = label;
        button.disabled = false;
      }
    });
}

function feedbackForm(element) {
  return element instanceof Element ? element.closest("form[data-feedback]") : null;
}

function updateFeedbackCount(form) {
  const message = form.querySelector('textarea[name="message"]');
  const count = form.querySelector("[data-feedback-count]");
  if (message && count) count.textContent = String(message.value.length);
}

// Opens the dialog with a fresh form, unless one is already there.
function openFeedbackModal(opener) {
  const modal = document.getElementById("feedbackModal");
  const body = modal?.querySelector("[data-feedback-body]");
  const template = document.getElementById("feedbackFormTemplate");
  if (!modal || !body || !template) return false;
  if (!body.querySelector("form[data-feedback]")) {
    body.replaceChildren(template.content.cloneNode(true));
    window.htmx?.process?.(body);
  }
  openModal(modal, opener);
  prepareFeedbackForm(body.querySelector("form[data-feedback]"));
  return true;
}

function setupFeedback() {
  document.addEventListener("click", (event) => {
    const link = event.target instanceof Element ? event.target.closest("[data-feedback-open]") : null;
    if (!link || event.ctrlKey || event.metaKey || event.shiftKey) return;
    if (openFeedbackModal(link)) event.preventDefault();
  });

  document.addEventListener("focusin", (event) => prepareFeedbackForm(feedbackForm(event.target)));

  document.addEventListener("input", (event) => {
    const form = feedbackForm(event.target);
    if (form) updateFeedbackCount(form);
  });

  // A form htmx swapped in (a refused message) needs its own proof.
  document.addEventListener("htmx:after:swap", () => {
    for (const form of document.querySelectorAll("form[data-feedback]")) {
      if (form.isConnected && form.closest(".modal.is-active, main")) prepareFeedbackForm(form);
    }
  });

  // Nothing is sent while the proof is still being solved.
  document.addEventListener(
    "submit",
    (event) => {
      const form = feedbackForm(event.target);
      if (form && form === event.target && form.dataset.pow === "solving") {
        event.preventDefault();
        event.stopImmediatePropagation();
      }
    },
    true,
  );
}
