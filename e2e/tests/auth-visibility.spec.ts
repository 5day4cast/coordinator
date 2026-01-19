import { test, expect, Page } from "@playwright/test";

// Helper to generate unique email for each test
function uniqueEmail(): string {
  return `test-${Date.now()}-${Math.random().toString(36).slice(2)}@example.com`;
}

// Helper to register with email
async function registerWithEmail(
  page: Page,
  email: string,
  password: string,
): Promise<void> {
  // Wait for WASM to initialize
  await page.waitForFunction(() => window.wasmInitialized === true, {
    timeout: 15000,
  });

  // Open register modal
  await page.locator("#registerNavClick").click();
  await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

  // Click email tab (should be default)
  await page.locator(".tabs li[data-target='registerEmail']").click();

  // Fill email registration form
  await page.locator("#registerEmailInput").fill(email);
  await page.locator("#registerPassword").fill(password);
  await page.locator("#registerPasswordConfirm").fill(password);

  // Click step 1 button to generate keys
  await page.locator("#emailRegisterStep1Button").click();

  // Wait for nsec to be displayed (WASM generates it)
  await expect(page.locator("#emailNsecDisplay")).toHaveValue(/^nsec1/, {
    timeout: 15000,
  });

  // Check the "I saved my key" checkbox
  await page.locator("#emailNsecSavedCheckbox").check();

  // Complete registration
  await page.locator("#emailRegisterStep2Button").click();

  // Should be logged in - logout container visible
  await expect(page.locator("#logoutContainer")).toBeVisible({
    timeout: 10000,
  });
}

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

    const email = uniqueEmail();
    const password = "testPassword123!";

    await registerWithEmail(page, email, password);

    // Auth buttons should be hidden
    await expect(page.locator("#authButtons")).toHaveClass(/is-hidden/);
  });

  test("logout returns user to unauthenticated state", async ({ page }) => {
    await page.goto("/");

    const email = uniqueEmail();
    const password = "testPassword123!";

    // Register and login
    await registerWithEmail(page, email, password);

    // Logout
    await page.locator("#logoutNavClick").click();

    // Should be back to unauthenticated state
    await expect(page.locator("#authButtons")).toBeVisible();
    await expect(page.locator("#loginNavClick")).toBeVisible();
  });
});
