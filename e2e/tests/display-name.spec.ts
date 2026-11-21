import { test, expect, Page } from "@playwright/test";

function uniqueUsername(): string {
  return `user${Date.now()}${Math.random().toString(36).slice(2, 6)}`;
}

async function registerWithUsername(
  page: Page,
  username: string,
  password: string,
): Promise<void> {
  // The wallet loads on demand (normally when a log-in dialog opens).
  await page.evaluate(() => window.initWasm());

  await page.locator("#registerNavClick").click();
  await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

  await page.locator(".tabs li[data-target='registerUsername']").click();

  await page.locator("#registerUsernameInput").fill(username);
  await page.locator("#registerPassword").fill(password);
  await page.locator("#registerPasswordConfirm").fill(password);
  await page.locator("#registerLightningAddress").fill(`${username}@mock-wallet.dev`);

  await page.locator("#usernameRegisterStep1Button").click();

  await expect(page.locator("#usernameNsecDisplay")).toHaveValue(/^nsec1/, {
    timeout: 15000,
  });

  await page.locator("#usernameNsecSavedCheckbox").check();

  await page.locator("#usernameRegisterStep2Button").click();

  await expect(page.locator("#logoutContainer")).toBeVisible({
    timeout: 10000,
  });
}

test("the sign-up password's rules turn green as they are met", async ({ page }) => {
  await page.goto("/");
  await page.locator("#registerNavClick").click();
  await page.locator(".tabs li[data-target='registerUsername']").click();
  const met = page.locator("#registerPasswordChecks .is-met");

  await page.locator("#registerPassword").fill("abc");
  await expect(met).toHaveCount(1);
  await page.locator("#registerPassword").fill("testPassword123!");
  await expect(met).toHaveCount(5);
});

test("a player picks the name leaderboards show, never their login", async ({ page }) => {
  await page.goto("/");
  const username = uniqueUsername();
  await registerWithUsername(page, username, "testPassword123!");

  await page.locator("#payoutsNavClick").click();
  await expect(page.locator("#displayName")).toContainText("short npub");

  await page.locator("#displayNameInput").fill(username);
  await page.locator("#saveDisplayName").click();
  await expect(page.locator("#displayNameError")).toContainText("login username");

  const name = `Storm ${username.slice(-6)}`;
  await page.locator("#displayNameInput").fill(name);
  await page.locator("#saveDisplayName").click();
  await expect(page.locator("#displayName")).toContainText(`You show as ${name}`);
});
