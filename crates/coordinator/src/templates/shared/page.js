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

function setupPage() {
  localizeTimes(document);
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

