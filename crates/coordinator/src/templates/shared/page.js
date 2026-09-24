// Small behaviours for server-rendered pages. They run on every htmx swap too.

// <time data-local> elements carry UTC; show them in the reader's time zone.
function localizeTimes(root) {
  for (const element of root.querySelectorAll("time[data-local]")) {
    const date = new Date(element.getAttribute("datetime"));
    if (Number.isNaN(date.getTime())) continue;
    const options =
      element.dataset.local === "time"
        ? { hour: "numeric", minute: "2-digit" }
        : { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" };
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
  document.body.addEventListener("htmx:load", (event) => {
    localizeTimes(event.detail.elt);
    markOwnRows(event.detail.elt);
  });

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
  document.body.addEventListener("htmx:afterSwap", (event) => {
    const modal = event.detail.target.closest?.(".modal");
    if (modal) window.openModal(modal);
  });
}

window.setOwnerTag = setOwnerTag;
window.setupPage = setupPage;
