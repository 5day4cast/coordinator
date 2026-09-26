import { test, expect, Page } from "@playwright/test";

// The log-in and sign-up dialogs follow the page's theme, and every piece of
// text in them is readable: WCAG AA, 4.5:1 or better against the dialog.

type Reading = { text: string; ratio: number; color: string; background: string };

async function contrasts(page: Page, dialog: string): Promise<Reading[]> {
  return page.evaluate((selector) => {
    const parse = (value: string) => {
      const parts = value.match(/[\d.]+/g)!.map(Number);
      return { rgb: parts.slice(0, 3), alpha: parts.length > 3 ? parts[3] : 1 };
    };
    const luminance = (rgb: number[]) => {
      const [r, g, b] = rgb.map((channel) => {
        const c = channel / 255;
        return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
      });
      return 0.2126 * r + 0.7152 * g + 0.0722 * b;
    };
    // The first opaque background behind an element.
    const backgroundOf = (element: Element | null): number[] => {
      for (let node = element; node; node = node.parentElement) {
        const { rgb, alpha } = parse(getComputedStyle(node).backgroundColor);
        if (alpha >= 0.99) return rgb;
      }
      return [255, 255, 255];
    };
    const root = document.querySelector(selector)!;
    const readings = [];
    for (const element of root.querySelectorAll(
      ".modal-card-title, .label, .help, p, a, .tabs li a",
    )) {
      const style = getComputedStyle(element);
      if (!element.textContent!.trim() || style.visibility === "hidden") continue;
      if ((element as HTMLElement).offsetParent === null) continue;
      const color = parse(style.color).rgb;
      const background = backgroundOf(element);
      const [light, dark] = [luminance(color), luminance(background)].sort((a, b) => b - a);
      readings.push({
        text: element.textContent!.trim().slice(0, 40),
        ratio: (light + 0.05) / (dark + 0.05),
        color: style.color,
        background: `rgb(${background.join(", ")})`,
      });
    }
    return readings;
  }, dialog);
}

for (const colorScheme of ["light", "dark"] as const) {
  test.describe(`dialogs in the ${colorScheme} theme`, () => {
    test.use({ colorScheme });

    for (const [opener, dialog] of [
      ["#loginNavClick", "#loginModal"],
      ["#registerNavClick", "#registerModal"],
    ]) {
      test(`${dialog} follows the page and its text reads at 4.5:1`, async ({ page }) => {
        await page.setViewportSize({ width: 1280, height: 800 });
        await page.goto("/");
        await expect(page.locator("html")).toHaveAttribute("data-theme", colorScheme);
        await page.locator(opener).click();
        await expect(page.locator(dialog)).toHaveClass(/is-active/);

        const card = await page
          .locator(`${dialog} .modal-card-body`)
          .evaluate((element) => getComputedStyle(element).backgroundColor);
        // The page's own background: the first opaque one from the body up.
        const page_background = await page.evaluate(() => {
          for (let node: Element | null = document.body; node; node = node.parentElement) {
            const value = getComputedStyle(node).backgroundColor;
            const alpha = value.match(/[\d.]+/g)!.map(Number)[3];
            if (alpha === undefined || alpha >= 0.99) return value;
          }
          return "rgb(255, 255, 255)";
        });
        const lightness = (value: string) =>
          value.match(/[\d.]+/g)!.slice(0, 3).map(Number).reduce((a, b) => a + b, 0) / 3;
        // Both light, or both dark.
        expect(lightness(card) > 128).toBe(lightness(page_background) > 128);

        const readings = await contrasts(page, dialog);
        expect(readings.length).toBeGreaterThan(3);
        const unreadable = readings.filter((reading) => reading.ratio < 4.5);
        expect(unreadable, JSON.stringify(unreadable, null, 2)).toEqual([]);
      });
    }
  });
}
