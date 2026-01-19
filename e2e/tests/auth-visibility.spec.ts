import { test, expect, Page } from "@playwright/test";

function uniqueUsername(): string {
  return `user${Date.now()}${Math.random().toString(36).slice(2, 6)}`;
}

async function registerWithUsername(
  page: Page,
  username: string,
  password: string,
): Promise<void> {
  await page.waitForFunction(() => window.wasmInitialized === true, {
    timeout: 15000,
  });

  await page.locator("#registerNavClick").click();
  await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

  await page.locator(".tabs li[data-target='registerUsername']").click();

  await page.locator("#registerUsernameInput").fill(username);
  await page.locator("#registerPassword").fill(password);
  await page.locator("#registerPasswordConfirm").fill(password);

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

test.describe("Public vs Authenticated Views", () => {
  test.beforeEach(async ({ page }) => {
    page.on("console", (msg) => {
      if (msg.type() === "error" || msg.text().includes("WASM")) {
        console.log(`Browser ${msg.type()}: ${msg.text()}`);
      }
    });
    page.on("pageerror", (error) => {
      console.log(`Page error: ${error.message}`);
    });
  });

  test("unauthenticated user can see competitions table", async ({ page }) => {
    await page.goto("/");

    await expect(page.locator(".navbar-brand strong")).toContainText(
      "Fantasy Weather",
    );
    await expect(page.locator("#competitionsDataTable")).toBeVisible();
    await expect(page.locator("#loginNavClick")).toBeVisible();
    await expect(page.locator("#registerNavClick")).toBeVisible();
  });

  test("unauthenticated user sees nav items", async ({ page }) => {
    await page.goto("/");

    await expect(page.locator("#allEntriesNavClick")).toBeVisible();
    await expect(page.locator("#payoutsNavClick")).toBeVisible();
    await expect(page.locator("#allCompetitionsNavClick")).toBeVisible();
  });

  test("user can register and login", async ({ page }) => {
    await page.goto("/");

    const username = uniqueUsername();
    const password = "testPassword123!";

    await registerWithUsername(page, username, password);

    await expect(page.locator("#authButtons")).toHaveClass(/is-hidden/);
  });

  test("logout returns user to unauthenticated state", async ({ page }) => {
    await page.goto("/");

    const username = uniqueUsername();
    const password = "testPassword123!";

    await registerWithUsername(page, username, password);

    await page.locator("#logoutNavClick").click();

    await expect(page.locator("#authButtons")).toBeVisible();
    await expect(page.locator("#loginNavClick")).toBeVisible();
  });
});
