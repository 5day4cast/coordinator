import { test, expect } from "@playwright/test";

test.describe("Public vs Authenticated Views", () => {
  // Capture console logs for debugging
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

    await expect(page.locator("h1")).toContainText("Fantasy Weather");
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

    // Wait for WASM to initialize
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    // Open register modal
    await page.locator("#registerNavClick").click();
    await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

    // Wait for private key generation (WASM)
    await expect(page.locator("#privateKeyDisplay")).toHaveValue(/^nsec1/, {
      timeout: 10000,
    });

    // Complete registration
    await page.locator("#privateKeySavedCheckbox").check();
    await page.locator("#registerStep1Button").click();

    // Should be logged in - logout container visible
    await expect(page.locator("#logoutContainer")).toBeVisible({
      timeout: 10000,
    });

    // Auth buttons should be hidden
    await expect(page.locator("#authButtons")).toHaveClass(/is-hidden/);
  });

  test("logout returns user to unauthenticated state", async ({ page }) => {
    await page.goto("/");

    // Wait for WASM to initialize
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    // Register and login
    await page.locator("#registerNavClick").click();
    await expect(page.locator("#privateKeyDisplay")).toHaveValue(/^nsec1/, {
      timeout: 10000,
    });
    await page.locator("#privateKeySavedCheckbox").check();
    await page.locator("#registerStep1Button").click();
    await expect(page.locator("#logoutContainer")).toBeVisible({
      timeout: 10000,
    });

    // Logout
    await page.locator("#logoutNavClick").click();

    // Should be back to unauthenticated state
    await expect(page.locator("#authButtons")).toBeVisible();
    await expect(page.locator("#loginNavClick")).toBeVisible();
  });
});
