// Uses the feedback form's proof preparation and the shared accessible modal behavior.
function setupMainnetSignup() {
  document.addEventListener("click", (event) => {
    const link = event.target instanceof Element ? event.target.closest("[data-mainnet-signup-open]") : null;
    if (!link || event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
    const modal = document.getElementById("mainnetSignupModal");
    const body = modal?.querySelector("[data-mainnet-signup-body]");
    const template = document.getElementById("mainnetSignupFormTemplate");
    if (!modal || !body || !template) return;
    event.preventDefault();
    if (!body.querySelector("form[data-mainnet-signup]")) {
      body.replaceChildren(template.content.cloneNode(true));
      window.htmx?.process?.(body);
    }
    openModal(modal, link);
    prepareFeedbackForm(body.querySelector("form[data-mainnet-signup]"));
  });
}
