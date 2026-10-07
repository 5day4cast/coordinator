// Browser telemetry (docs/REQUEST_CONTEXT.md), on only when the page has
// <meta name="telemetry" content="on">. It sends page timings, clicks on
// buttons and links, form ids, htmx request timings, script errors and named
// milestones (window.fdcMark) to /api/v1/telemetry with sendBeacon.
//
// - It never reads what anyone types: no input, textarea or select values,
//   no contenteditable text, and nothing inside [data-telemetry="off"].
// - The session id is random per tab, kept in sessionStorage; no cookies.
// - Nothing is retried, and every handler is wrapped so it cannot break the page.
function setupTelemetry() {
  try {
    startTelemetry();
  } catch (_) {}
}

function startTelemetry() {
  const ENDPOINT = "/api/v1/telemetry";
  const MAX_EVENTS = 50;
  const MAX_BYTES = 15 * 1024;
  const MAX_ERRORS = 20;

  if (document.querySelector('meta[name="telemetry"]')?.content !== "on") return;
  if (typeof navigator.sendBeacon !== "function") return;

  const rid = document.querySelector('meta[name="request-id"]')?.content || null;
  let sid = null;
  try {
    sid = sessionStorage.getItem("fdc.sid");
  } catch (_) {}
  if (!/^[A-Za-z0-9_-]{22}$/.test(sid || "")) {
    const bytes = crypto.getRandomValues(new Uint8Array(16));
    sid = btoa(String.fromCharCode(...bytes))
      .replace(/\+/g, "-")
      .replace(/\//g, "_")
      .replace(/=+$/, "");
    try {
      sessionStorage.setItem("fdc.sid", sid);
    } catch (_) {}
  }

  const queue = [];
  let errors = 0;
  const cut = (value, max = 200) => (value == null ? undefined : String(value).slice(0, max));

  function flush() {
    try {
      while (queue.length) {
        let events = queue.splice(0, MAX_EVENTS);
        let body = JSON.stringify({ sid, rid, events });
        while (new Blob([body]).size > MAX_BYTES && events.length > 1) {
          queue.unshift(...events.splice(Math.ceil(events.length / 2)));
          body = JSON.stringify({ sid, rid, events });
        }
        navigator.sendBeacon(ENDPOINT, new Blob([body], { type: "application/json" }));
      }
    } catch (_) {}
  }

  function record(ev, fields) {
    try {
      queue.push({ ev, t: Math.round(performance.now()), page: location.pathname, ...fields });
      if (queue.length >= MAX_EVENTS) flush();
    } catch (_) {}
  }

  window.fdcMark = (name) => record("mark", { name: cut(name, 64) });

  // A same-site referrer as its path, any other as its host.
  function referrer() {
    try {
      if (!document.referrer) return undefined;
      const url = new URL(document.referrer);
      return url.origin === location.origin ? url.pathname : url.host;
    } catch (_) {
      return undefined;
    }
  }

  function pageView() {
    const nav = performance.getEntriesByType?.("navigation")?.[0];
    record("page_view", {
      ref: referrer(),
      ttfb: nav ? Math.round(nav.responseStart) : undefined,
      dcl: nav ? Math.round(nav.domContentLoadedEventEnd) : undefined,
      load: nav ? Math.round(nav.loadEventEnd) : undefined,
    });
  }
  if (document.readyState === "complete") pageView();
  else addEventListener("load", () => setTimeout(pageView, 0));

  // Largest contentful paint, cumulative layout shift, and the slowest interaction.
  const vitals = { lcp: undefined, cls: 0, inp: undefined };
  const observe = (type, handle, options = {}) => {
    try {
      new PerformanceObserver((list) => list.getEntries().forEach(handle)).observe({
        type,
        buffered: true,
        ...options,
      });
    } catch (_) {}
  };
  observe("largest-contentful-paint", (entry) => (vitals.lcp = Math.round(entry.startTime)));
  observe("layout-shift", (entry) => {
    if (!entry.hadRecentInput) vitals.cls += entry.value;
  });
  observe(
    "event",
    (entry) => (vitals.inp = Math.max(vitals.inp ?? 0, Math.round(entry.duration))),
    { durationThreshold: 40 },
  );

  const quiet = (element) => element.closest('[data-telemetry="off"]');

  document.addEventListener(
    "click",
    (event) => {
      try {
        const target =
          event.target instanceof Element &&
          event.target.closest("button, a, [role=button], [data-track]");
        if (!target || quiet(target)) return;
        const fields = { el: target.tagName.toLowerCase() };
        if (target.id) fields.id = cut(target.id);
        if (target.dataset.track) fields.track = cut(target.dataset.track);
        if (target.matches("button, a, [role=button]") && !target.isContentEditable) {
          const text = (target.textContent || "").replace(/\s+/g, " ").trim();
          if (text) fields.text = text.slice(0, 40);
        }
        record("click", fields);
      } catch (_) {}
    },
    true,
  );

  document.addEventListener(
    "submit",
    (event) => {
      try {
        const form = event.target;
        if (!(form instanceof Element) || quiet(form)) return;
        record("submit", { form: cut(form.id) || undefined });
      } catch (_) {}
    },
    true,
  );

  // htmx 4 names: htmx:config:request before a request is built, and
  // htmx:finally:request once it has an answer or failed.
  document.addEventListener("htmx:config:request", (event) => {
    try {
      const ctx = event.detail.ctx;
      const url = new URL(ctx.request.action, document.baseURI);
      if (url.origin !== location.origin) return;
      ctx.request.headers["X-Session-Id"] = sid;
      ctx.fdcStarted = performance.now();
    } catch (_) {}
  });
  document.addEventListener("htmx:finally:request", (event) => {
    try {
      const ctx = event.detail.ctx;
      if (ctx.fdcStarted == null) return;
      record("htmx", {
        verb: ctx.request.method,
        path: new URL(ctx.request.action, document.baseURI).pathname,
        status: ctx.response?.status ?? 0,
        ms: Math.round(performance.now() - ctx.fdcStarted),
        rid: ctx.response?.headers?.get?.("X-Request-Id") || undefined,
      });
    } catch (_) {}
  });

  // The script's file name only; its query and the page's origin say nothing more.
  const file = (src) => cut(String(src || "").split(/[?#]/)[0].split("/").pop());
  addEventListener("error", (event) => {
    if (!(event instanceof ErrorEvent) || ++errors > MAX_ERRORS) return;
    record("js_error", { msg: cut(event.message), src: file(event.filename), line: event.lineno });
  });
  addEventListener("unhandledrejection", (event) => {
    if (++errors > MAX_ERRORS) return;
    record("js_error", { msg: cut(event.reason?.message ?? event.reason) });
  });

  setInterval(() => {
    if (queue.length) flush();
  }, 10_000);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") flush();
  });
  addEventListener("pagehide", () => {
    record("vitals", { lcp: vitals.lcp, cls: Math.round(vitals.cls * 1000) / 1000, inp: vitals.inp });
    flush();
  });
}
