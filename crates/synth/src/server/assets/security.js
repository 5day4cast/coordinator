// What htmx 4 may do with the HTML it swaps in. htmx hands every string it parses to a Trusted
// Types policy, and runs hx-on handlers and js: values through the Function constructors it is
// given; this extension, registered before htmx processes the page, gives it both.
//
// - The "htmx" policy is the only one synth's Content-Security-Policy allows. It passes HTML
//   through (htmx fetches from synth only) and turns every <script> in swapped content into an
//   empty one.
// - The constructors refuse to run code, so no attribute in a response runs even without the
//   policy's help.
(() => {
  function refuse() {
    throw new Error('synth runs no inline JavaScript');
  }
  const rules = { createHTML: (html) => html, createScript: () => '' };
  const policy = window.trustedTypes ? window.trustedTypes.createPolicy('htmx', rules) : rules;
  window.htmx.registerExtension('synth-security', {
    init(api) {
      api.initSecurity(policy, refuse, refuse);
    },
  });
})();
