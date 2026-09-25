// What htmx 4 may do with the HTML it swaps in, as an extension registered
// when this script loads, before htmx processes the page.
//
// - Trusted Types: the page's Content-Security-Policy requires them for every
//   script sink and allows one policy, "htmx", created here. htmx parses
//   responses from this site only (mode same-origin) through it; nothing else
//   on the page can create HTML or script from a string.
// - Scripts in swapped content are never re-created with their text
//   (createScript returns nothing), and their src cannot be set without a
//   TrustedScriptURL, which no policy here makes.
// - hx-on, trigger filters and js: values would run through htmx's Function
//   constructor. The policy forbids eval anyway; these constructors refuse too,
//   so no attribute in a response can run code even if the policy were lost.

function refuseCode() {
  return {
    call() {
      throw new Error("htmx: inline JavaScript is disabled on this site");
    },
  };
}

window.htmx?.registerExtension("fw-security", {
  init(api) {
    const policy = window.trustedTypes?.createPolicy("htmx", {
      createHTML: (html) => html,
      createScript: () => "",
    }) ?? { createHTML: (html) => html, createScript: () => "" };
    api.initSecurity(policy, refuseCode, refuseCode);
  },
});
