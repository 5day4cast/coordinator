// Records the help page's "How it works" video: a phone-sized walk through the player pages,
// with a ring and a caption on each thing as it comes up. It plays the pages
// `player_ui_fixtures` exports (the real templates with sample data), served from a local
// directory, so it needs no running coordinator. Frames come from Chrome's screencast at the
// phone's pixel density (Playwright's own recorder films at CSS pixels), and ffmpeg makes the
// MP4 the help page plays. `just help-video` runs it.
//
//   node video/how-it-works.cjs <fixtures-dir> <out.mp4>
const { chromium } = require("@playwright/test");
const { createServer } = require("node:http");
const { readFile, writeFile, mkdtemp, rm } = require("node:fs/promises");
const { execFileSync } = require("node:child_process");
const path = require("node:path");
const os = require("node:os");

const [fixtures, output] = process.argv.slice(2);
if (!fixtures || !output) {
  console.error("usage: node video/how-it-works.cjs <fixtures-dir> <out.mp4>");
  process.exit(2);
}

const VIEWPORT = { width: 390, height: 844 };
const SCALE = 2;
// The width the MP4 is encoded at: sharp on a phone, a few hundred kilobytes for a minute.
const VIDEO_WIDTH = 540;

const TYPES = { ".html": "text/html", ".css": "text/css", ".js": "text/javascript", ".svg": "image/svg+xml" };

function serve(root) {
  const server = createServer(async (request, response) => {
    const file = path.join(root, decodeURIComponent(new URL(request.url, "http://x").pathname));
    if (!file.startsWith(path.resolve(root))) return response.writeHead(403).end();
    try {
      const body = await readFile(file);
      response.writeHead(200, { "content-type": TYPES[path.extname(file)] ?? "application/octet-stream" });
      response.end(body);
    } catch {
      response.writeHead(404).end();
    }
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server)));
}

// The overlay: a ring around what is being shown, a caption bubble pointing at it, and a dot
// where a tap lands, in the help page's callout teal (the Pay button's, a step darker). Drawn
// into the page, so it is in the recording.
const OVERLAY_CSS = `
  .hiw-ring { position: fixed; z-index: 2147483000; pointer-events: none; border: 3px solid #00806c;
    border-radius: 10px; box-shadow: 0 0 0 5px rgba(0, 128, 108, .22); transition: all .35s ease; }
  .hiw-bubble { position: fixed; z-index: 2147483001; pointer-events: none; max-width: 330px;
    padding: 10px 14px; border-radius: 12px; background: #111827; color: #fff;
    font: 600 16px/1.35 system-ui, sans-serif; box-shadow: 0 6px 20px rgba(0,0,0,.3); }
  .hiw-bubble::before { content: ""; position: absolute; left: var(--arrow-x, 50%);
    width: 14px; height: 14px; background: #111827; transform: translateX(-50%) rotate(45deg); }
  .hiw-bubble.is-below::before { top: -6px; }
  .hiw-bubble.is-above::before { bottom: -6px; }
  .hiw-bubble.is-centered::before { display: none; }
  .hiw-tap { position: fixed; z-index: 2147483002; pointer-events: none; width: 34px; height: 34px;
    margin: -17px 0 0 -17px; border-radius: 50%; background: rgba(0, 128, 108, .45);
    border: 2px solid #fff; }
`;

async function open(page, base, name) {
  await page.goto(`${base}/${name}-light.html`);
  await page.addStyleTag({ content: OVERLAY_CSS });
  await page.waitForTimeout(400);
}

// Ring `selector` (the nth match) and point a caption at it; with no selector, a caption in the
// middle of the screen.
async function show(page, text, selector, { nth = 0, hold = 2800 } = {}) {
  await page.evaluate(
    async ({ text, selector, nth }) => {
      document.querySelectorAll(".hiw-ring, .hiw-bubble").forEach((element) => element.remove());
      const bubble = document.createElement("div");
      bubble.className = "hiw-bubble";
      bubble.textContent = text;
      document.body.append(bubble);
      const target = selector && document.querySelectorAll(selector)[nth];
      if (!target) {
        bubble.classList.add("is-centered");
        bubble.style.left = `${(innerWidth - bubble.offsetWidth) / 2}px`;
        bubble.style.top = `${innerHeight * 0.4}px`;
        return;
      }
      // Scroll only what is off screen: scrolling a dialog's own container shifts the dialog.
      const seen = target.getBoundingClientRect();
      if (seen.top < 70 || seen.bottom > innerHeight - 140) {
        target.scrollIntoView({ block: "center" });
        await new Promise((resolve) => setTimeout(resolve, 150));
      }
      const box = target.getBoundingClientRect();
      const ring = document.createElement("div");
      ring.className = "hiw-ring";
      Object.assign(ring.style, {
        left: `${box.left - 6}px`, top: `${box.top - 6}px`,
        width: `${box.width + 12}px`, height: `${box.height + 12}px`,
      });
      document.body.append(ring);
      const below = box.bottom + 20 + bubble.offsetHeight < innerHeight;
      bubble.classList.add(below ? "is-below" : "is-above");
      const left = Math.min(Math.max(12, box.left + box.width / 2 - bubble.offsetWidth / 2),
        innerWidth - bubble.offsetWidth - 12);
      bubble.style.left = `${left}px`;
      bubble.style.top = below ? `${box.bottom + 16}px` : `${box.top - bubble.offsetHeight - 16}px`;
      bubble.style.setProperty("--arrow-x",
        `${Math.min(Math.max(16, box.left + box.width / 2 - left), bubble.offsetWidth - 16)}px`);
    },
    { text, selector, nth },
  );
  await page.waitForTimeout(hold);
}

// A tap: the last caption goes, a dot lands on the element, then the click.
async function tap(page, locator) {
  await page.evaluate(() => document.querySelectorAll(".hiw-ring, .hiw-bubble").forEach((element) => element.remove()));
  const box = await locator.boundingBox();
  await page.evaluate(({ x, y }) => {
    const dot = document.createElement("div");
    dot.className = "hiw-tap";
    Object.assign(dot.style, { left: `${x}px`, top: `${y}px` });
    document.body.append(dot);
    setTimeout(() => dot.remove(), 650);
  }, { x: box.x + box.width / 2, y: box.y + box.height / 2 });
  await page.waitForTimeout(300);
  await locator.click();
  await page.waitForTimeout(700);
}

async function record(base, directory) {
  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: VIEWPORT, deviceScaleFactor: SCALE });
  const page = await context.newPage();
  await open(page, base, "home");

  // Chrome sends a frame whenever the page changes; each is shown until the next one.
  const frames = [];
  const cdp = await context.newCDPSession(page);
  cdp.on("Page.screencastFrame", ({ data, metadata, sessionId }) => {
    frames.push({ data, time: metadata.timestamp });
    cdp.send("Page.screencastFrameAck", { sessionId }).catch(() => {});
  });
  await cdp.send("Page.startScreencast", {
    format: "jpeg",
    quality: 90,
    maxWidth: VIEWPORT.width * SCALE,
    maxHeight: VIEWPORT.height * SCALE,
  });

  await scenes(page, base);

  await cdp.send("Page.stopScreencast");
  await browser.close();

  let list = "";
  for (const [index, frame] of frames.entries()) {
    const file = `frame-${String(index).padStart(5, "0")}.jpg`;
    await writeFile(path.join(directory, file), Buffer.from(frame.data, "base64"));
    const next = frames[index + 1];
    list += `file '${file}'\nduration ${next ? Math.max(0.001, next.time - frame.time).toFixed(3) : 1}\n`;
  }
  // The concat demuxer takes the last file's duration only if the file is listed again.
  list += `file 'frame-${String(frames.length - 1).padStart(5, "0")}.jpg'\n`;
  await writeFile(path.join(directory, "frames.txt"), list);
  execFileSync("ffmpeg", [
    "-y", "-v", "error", "-f", "concat", "-safe", "0", "-i", path.join(directory, "frames.txt"),
    "-vf", `fps=25,scale=${VIDEO_WIDTH}:-2:flags=lanczos`,
    "-c:v", "libx264", "-preset", "slow", "-crf", "31", "-pix_fmt", "yuv420p",
    "-movflags", "+faststart", "-an", path.resolve(output),
  ]);
  return frames.length;
}

async function scenes(page, base) {
  await show(page, "Pick a competition that's open. Price is what it costs you; Win is what first place takes.", ".featured-card", { hold: 3800 });
  await tap(page, page.locator(".featured-card a.button"));

  await open(page, base, "entry");
  await show(page, "Entries close at this time. Your picks lock then.", ".entry-facts > div", { nth: 0 });
  await show(page, "The price is everything you pay.", ".price-details summary");
  await tap(page, page.locator(".price-details summary"));
  await show(page, "Tap it to see the fees it's made of.", ".price-lines");
  await tap(page, page.locator(".price-details summary"));
  await page.locator(".entry-facts .tip").first().focus();
  await show(page, "Tap any ? for what a term means.", ".entry-facts .tip", { hold: 2600 });
  await page.locator(".entry-facts .tip").first().blur();

  const high = page.locator(".pick-row").nth(0);
  await show(page, "Each airport has a forecast for the high, the low and the wind.", ".pick-row", { hold: 3000 });
  await show(page, "Pick where the reading lands: Under, Par (the middle range) or Over.", ".pick-options", { hold: 3200 });
  await tap(page, high.locator(".pick-option").nth(1));
  await tap(page, page.locator(".pick-row").nth(1).locator(".pick-option").nth(2));
  await tap(page, page.locator(".pick-row").nth(2).locator(".pick-option").nth(0));
  await show(page, "Pick as many as you like. A wrong or skipped pick costs nothing.", ".station-picks", { hold: 3000 });
  // Choosing a pick again clears it (entry_form.js; these pages run no scripts).
  await tap(page, high.locator(".pick-option").nth(1));
  await high.locator("input:checked").evaluateAll((inputs) => inputs.forEach((input) => { input.checked = false; }));
  await show(page, "Tap a pick again to clear it.", ".pick-options", { hold: 2400 });
  await show(page, "Pay the Lightning invoice and you're in.", "#submitEntry", { hold: 3000 });

  await open(page, base, "leaderboard");
  await show(page, "Once entries close, follow the leaderboard.", ".leaderboard-table", { hold: 3000 });
  await show(page, "That's you.", "tr.is-own", { hold: 2200 });

  await open(page, base, "picks");
  await show(page, "Tap Picks to see each reading so far...", ".pick-reading", { nth: 3, hold: 3000 });
  await show(page, "...and the points it scores if the window ended now.", ".pick-points", { nth: 1, hold: 3000 });

  await open(page, base, "help");
  await show(page, "The full rules are on this page.", null, { hold: 2500 });
}

(async () => {
  const server = await serve(path.resolve(fixtures));
  const base = `http://127.0.0.1:${server.address().port}`;
  const directory = await mkdtemp(path.join(os.tmpdir(), "how-it-works-"));
  try {
    const frames = await record(base, directory);
    console.log(`recorded ${output} from ${frames} frames`);
  } finally {
    server.close();
    await rm(directory, { recursive: true, force: true });
  }
})().catch((error) => {
  console.error(error);
  process.exit(1);
});
