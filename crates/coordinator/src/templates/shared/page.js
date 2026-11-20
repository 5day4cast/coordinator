// Small behaviours for server-rendered pages. They run on every htmx swap too.

// <time data-local> elements carry UTC; show them in the reader's time zone. One marked
// data-zone (a window's end) also names the zone, "Sep 30, 10:02 PM EDT", so the reader
// knows the times are their own. A window's end marked data-since (its start) keeps its date
// when it falls on a later day in the reader's zone: "Oct 2, 8:00 PM – Oct 3, 8:00 AM EDT".
function localizeTimes(root) {
  for (const element of root.querySelectorAll("time[data-local]")) {
    const date = new Date(element.getAttribute("datetime"));
    if (Number.isNaN(date.getTime())) continue;
    const since = element.dataset.since ? new Date(element.dataset.since) : null;
    const nextDay = since !== null && since.toDateString() !== date.toDateString();
    const options =
      element.dataset.local === "time" && !nextDay
        ? { hour: "numeric", minute: "2-digit" }
        : element.dataset.local === "weekday"
          ? { weekday: "short", hour: "numeric", minute: "2-digit" }
          : { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" };
    if ("zone" in element.dataset) options.timeZoneName = "short";
    element.textContent = date.toLocaleString(undefined, options);
    element.title = date.toLocaleString();
  }
}

// Rows tagged with the signed-in account's hash show a "You" badge.
let ownerTag = null;

function markOwnRows(root) {
  for (const row of root.querySelectorAll("[data-owner]")) {
    row.classList.toggle("is-own", Boolean(ownerTag) && row.dataset.owner === ownerTag);
  }
}

async function setOwnerTag(npub) {
  if (npub) {
    const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(npub));
    ownerTag = [...new Uint8Array(digest)]
      .map((byte) => byte.toString(16).padStart(2, "0"))
      .join("")
      .slice(0, 16);
  } else {
    ownerTag = null;
  }
  markOwnRows(document);
}

// "7 h 52 min" for `seconds` from now, as `format::duration` writes it.
function untilText(seconds) {
  const minutes = Math.max(0, Math.floor(seconds / 60));
  const [days, hours, mins] = [Math.floor(minutes / 1440), Math.floor(minutes / 60) % 24, minutes % 60];
  if (days === 0 && hours === 0) return mins === 0 ? "under a minute" : `${mins} min`;
  if (days === 0) return mins === 0 ? `${hours} h` : `${hours} h ${mins} min`;
  if (days === 1) return hours === 0 ? "1 day" : `1 day ${hours} h`;
  return `${days} days`;
}

// Countdowns marked data-countdown (a unix time) stay current while the page is open. One that
// runs out says so once, with `fw:countdown-ended` from the body: the lobby reloads, and the
// entry form stops taking picks.
function tickCountdowns() {
  const now = Date.now() / 1000;
  for (const element of document.querySelectorAll("[data-countdown]")) {
    const left = Number(element.dataset.countdown) - now;
    if (left > 0) {
      element.textContent = untilText(left);
    } else if (!element.dataset.ended) {
      element.dataset.ended = "true";
      element.textContent = "now";
      document.body.dispatchEvent(new CustomEvent("fw:countdown-ended", { detail: { element } }));
    }
  }
}

// The header link for the section the reader is in. The Lobby's is the lobby alone; a
// competition's own pages are none of the four.
function markCurrentNav(path = location.pathname) {
  for (const link of document.querySelectorAll(".site-nav-link")) {
    const href = link.getAttribute("href");
    const current = href === "/competitions"
      ? path === "/" || path === "/competitions"
      : path.startsWith(href);
    if (current) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
}

function setupPage() {
  localizeTimes(document);
  markCurrentNav();
  setInterval(tickCountdowns, 10_000);
  // A navigation changes the address after its click, and Back without one.
  document.addEventListener("click", (event) => {
    const link = event.target.closest?.("a[hx-push-url][href]");
    if (link) markCurrentNav(new URL(link.href, location.href).pathname);
  });
  window.addEventListener("popstate", () => markCurrentNav());
  // htmx processes every swapped-in element, and the page itself at start.
  document.addEventListener("htmx:after:process", (event) => {
    localizeTimes(event.target);
    markOwnRows(event.target);
    showKeymeldTrust();
  });

  // A tip or a disclosure inside a clickable row opens without opening the row.
  document.addEventListener("click", (event) => {
    if (event.target.closest?.("tr.is-clickable :is([data-tip], details)")) event.stopPropagation();
  }, true);

  document.addEventListener("click", async (event) => {
    const button = event.target.closest?.("[data-copy]");
    if (!button) return;
    // A copy button inside a clickable row copies without opening the row.
    event.preventDefault();
    event.stopPropagation();
    try {
      await navigator.clipboard.writeText(button.dataset.copy);
      button.textContent = "Copied";
      setTimeout(() => (button.textContent = "Copy"), 1500);
    } catch (error) {
      console.error("Copy failed:", error);
    }
  }, true);

  // Content swapped into a modal (an entry's picks) opens that modal.
  document.addEventListener("htmx:after:swap", (event) => {
    const modal = event.detail.ctx.target?.closest?.(".modal");
    if (modal) openModal(modal);
  });
}

