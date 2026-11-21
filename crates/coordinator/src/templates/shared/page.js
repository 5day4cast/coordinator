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

// "04:59:12", or "1d 04:59:12", as `format::clock` writes it.
function clockText(seconds) {
  const left = Math.max(0, Math.floor(seconds));
  const days = Math.floor(left / 86_400);
  const rest = left % 86_400;
  const pad = (value) => String(value).padStart(2, "0");
  const time = `${pad(Math.floor(rest / 3600))}:${pad(Math.floor(rest / 60) % 60)}:${pad(rest % 60)}`;
  return days > 0 ? `${days}d ${time}` : time;
}

// Countdowns marked data-countdown (a unix time) stay current while the page is open, to the
// second where marked data-clock. One that
// runs out says so once, with `fw:countdown-ended` from the body: the lobby reloads, and the
// entry form stops taking picks.
function tickCountdowns() {
  const now = Date.now() / 1000;
  for (const element of document.querySelectorAll("[data-countdown]")) {
    const left = Number(element.dataset.countdown) - now;
    if (left > 0) {
      element.textContent = element.hasAttribute("data-clock") ? clockText(left) : untilText(left);
    } else if (!element.dataset.ended) {
      element.dataset.ended = "true";
      element.textContent = element.hasAttribute("data-clock") ? clockText(0) : "now";
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

// The satellite loop behind the page, at half speed, near the pace of NOAA's own page. Readers
// who ask for less motion or less data never download it.
function startSatellite() {
  const video = document.querySelector("video.satellite");
  const still = matchMedia("(prefers-reduced-motion: reduce)").matches || navigator.connection?.saveData;
  if (!video || still) return;
  // Loading resets the speed to the default one.
  video.defaultPlaybackRate = 0.5;
  video.src = video.dataset.src;
  video.play().catch(() => {});
}

// The lobby's WeatherStar frame, where the page shows it (desktop). A hidden frame with a src
// would still load, so its src waits in data-src.
function loadWeatherstar() {
  for (const frame of document.querySelectorAll("iframe.weatherstar:not([src])")) {
    if (getComputedStyle(frame).display !== "none") frame.src = frame.dataset.src;
  }
}

function setupPage() {
  startSatellite();
  loadWeatherstar();
  localizeTimes(document);
  markCurrentNav();
  setInterval(tickCountdowns, 1_000);
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
    loadWeatherstar();
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

