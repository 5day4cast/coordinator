// Sorting is local to the upcoming list and survives its public 30-second refresh.
let competitionSort = { key: "start", descending: false };

function compareCompetitions(a, b, sort) {
  const left = a.dataset[sort.key];
  const right = b.dataset[sort.key];
  // An unknown prize stays last in either direction.
  if (left === undefined || right === undefined) {
    if (left !== right) return left === undefined ? 1 : -1;
  } else {
    const delta = BigInt(left) - BigInt(right);
    if (delta !== 0n) return (delta < 0n ? -1 : 1) * (sort.descending ? -1 : 1);
  }
  const start = BigInt(a.dataset.start) - BigInt(b.dataset.start);
  return start < 0n ? -1 : start > 0n ? 1
    : a.dataset.competitionId.localeCompare(b.dataset.competitionId);
}

function sortCompetitionLists() {
  for (const list of document.querySelectorAll(".competition-list[data-sortable]")) {
    const rows = [...list.querySelectorAll(".competition-row")];
    rows.sort((a, b) => compareCompetitions(a, b, competitionSort));
    for (const row of rows) list.append(row);
    for (const button of list.querySelectorAll("[data-sort]")) {
      const selected = button.dataset.sort === competitionSort.key;
      button.querySelector(".sort-direction").textContent = selected
        ? competitionSort.descending ? "↓" : "↑" : "↕";
      const descending = selected ? !competitionSort.descending : button.dataset.sort === "prize";
      const order = button.dataset.sort === "start"
        ? descending ? "latest first" : "soonest first"
        : descending ? "highest first" : "lowest first";
      const label = { start: "Starts", fee: "Entry fee", prize: "Prizes" }[button.dataset.sort];
      button.setAttribute("aria-label", `${label}: sort ${order}`);
      button.setAttribute("aria-pressed", String(selected));
    }
  }
}

function setupCompetitions() {
  // Keep private counts in memory for this visit. Public list polling reuses them,
  // so a Nostr extension is not asked to sign every automatic refresh.
  let counts = null;
  let state = "idle";
  let signer = null;
  let generation = 0;
  const reset = () => {
    generation += 1;
    counts = null;
    state = "idle";
    signer = null;
  };
  const paint = () => {
    for (const element of document.querySelectorAll("[data-player-entries]")) {
      const limit = element.dataset.entryLimit;
      element.textContent = !isLoggedIn() ? `${limit} max per player`
        : state === "ready" ? `Your entries: ${counts[element.dataset.playerEntries] ?? 0} / ${limit}`
        : state === "failed" ? `${limit} max per player · Your count unavailable`
        : `${limit} max per player · Loading yours…`;
    }
  };
  const refresh = async () => {
    sortCompetitionLists();
    if (!document.querySelector("[data-player-entries]")) {
      reset();
      return;
    }
    if (!isLoggedIn()) {
      reset();
      paint();
      return;
    }
    if (signer && signer !== session.nostrClient) reset();
    paint();
    if (state !== "idle") return;
    state = "loading";
    signer = session.nostrClient;
    const requested = generation;
    const active = () => requested === generation && isLoggedIn() && signer === session.nostrClient;
    try {
      const client = new AuthorizedClient(signer, window.location.origin);
      const response = await client.get(`${window.location.origin}/competitions/entry-counts`, {
        isActive: active,
        cache: "no-store",
      });
      const result = await response.json();
      if (!active()) return;
      counts = result;
      state = "ready";
    } catch (error) {
      if (!active()) return;
      state = "failed";
      console.error("Could not load your competition entry counts:", error);
    }
    paint();
  };
  document.addEventListener("click", (event) => {
    const button = event.target.closest?.(".competition-sort");
    if (!button) return;
    const key = button.dataset.sort;
    competitionSort = {
      key,
      descending: key === competitionSort.key ? !competitionSort.descending : key === "prize",
    };
    sortCompetitionLists();
  });
  document.addEventListener("htmx:after:swap", refresh);
  document.body.addEventListener("fw:login", () => { reset(); refresh(); });
  document.body.addEventListener("fw:logout", () => { reset(); paint(); });
  refresh();
}
