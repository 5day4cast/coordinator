import { test, expect } from "@playwright/test";

// The footer's Feedback dialog sends a message with its proof of work, and the
// thanks say it reached the team. Without JavaScript, /feedback does the same.

test("feedback is sent from the footer's dialog", async ({ page }) => {
  await page.goto("/");
  await page.locator("footer [data-feedback-open]").click();
  const dialog = page.locator("#feedbackModal");
  await expect(dialog).toHaveClass(/is-active/);

  const message = `e2e feedback ${Date.now()}`;
  await dialog.locator('textarea[name="message"]').fill(message);
  await expect(dialog.locator("[data-feedback-count]")).toHaveText(String(message.length));
  await dialog.locator('input[name="contact"]').fill("npub1example");
  await dialog.locator('button[type="submit"]').click();

  await expect(dialog.getByText("Your message reached the 5day4cast team.")).toBeVisible({
    timeout: 15_000,
  });
});

test.describe("without JavaScript", () => {
  test.use({ javaScriptEnabled: false });

  test("the feedback page sends a message", async ({ page }) => {
    await page.goto("/feedback");
    await page.locator('textarea[name="message"]').fill(`e2e without script ${Date.now()}`);
    await page.locator('button[type="submit"]').click();
    await expect(page.getByText("Your message reached the 5day4cast team.")).toBeVisible();
  });
});
