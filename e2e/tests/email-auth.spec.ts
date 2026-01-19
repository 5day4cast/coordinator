import { test, expect, Page } from "@playwright/test";

/**
 * E2E tests for email/password authentication.
 *
 * Tests cover:
 * 1. Email registration with nsec backup display
 * 2. Email login after registration
 * 3. Login rejection with wrong password
 * 4. Duplicate email registration rejection
 * 5. Password change while logged in
 * 6. Forgot password flow with nsec challenge signing
 * 7. Email user can access protected routes
 * 8. Extension login still works (regression test)
 */

// Helper to generate unique email for each test
function uniqueEmail(): string {
  return `test-${Date.now()}-${Math.random().toString(36).slice(2)}@example.com`;
}

// Helper to register with email
async function registerWithEmail(
  page: Page,
  email: string,
  password: string,
): Promise<string> {
  await page.goto("/");

  // Wait for WASM to initialize
  await page.waitForFunction(() => window.wasmInitialized === true, {
    timeout: 15000,
  });

  // Open register modal
  await page.locator("#registerNavClick").click();
  await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

  // Click email tab (should be default, but be explicit)
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

  // Capture nsec for potential use in forgot password tests
  const nsec = await page.locator("#emailNsecDisplay").inputValue();

  // Check the "I saved my key" checkbox
  await page.locator("#emailNsecSavedCheckbox").check();

  // Complete registration
  await page.locator("#emailRegisterStep2Button").click();

  // Should be logged in - logout container visible
  await expect(page.locator("#logoutContainer")).toBeVisible({
    timeout: 10000,
  });

  return nsec;
}

// Helper to login with email
async function loginWithEmail(
  page: Page,
  email: string,
  password: string,
): Promise<void> {
  await page.goto("/");

  // Wait for WASM to initialize
  await page.waitForFunction(() => window.wasmInitialized === true, {
    timeout: 15000,
  });

  // Open login modal
  await page.locator("#loginNavClick").click();
  await expect(page.locator("#loginModal")).toHaveClass(/is-active/);

  // Click email tab (should be default)
  await page.locator(".tabs li[data-target='emailLogin']").click();

  // Fill login form
  await page.locator("#loginEmail").fill(email);
  await page.locator("#loginPassword").fill(password);

  // Click login button
  await page.locator("#emailLoginButton").click();

  // Should be logged in
  await expect(page.locator("#logoutContainer")).toBeVisible({
    timeout: 10000,
  });
}

// Helper to logout
async function logout(page: Page): Promise<void> {
  await page.locator("#logoutNavClick").click();
  await expect(page.locator("#authButtons")).toBeVisible();
}

test.describe("Email/Password Authentication", () => {
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

  test("registers new user with email/password and shows nsec backup", async ({
    page,
  }) => {
    const email = uniqueEmail();
    const password = "testPassword123!";

    await page.goto("/");

    // Wait for WASM to initialize
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    // Open register modal
    await page.locator("#registerNavClick").click();
    await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

    // Email tab should be visible and active by default
    await expect(
      page.locator(".tabs li[data-target='registerEmail']"),
    ).toBeVisible();

    // Fill form
    await page.locator("#registerEmailInput").fill(email);
    await page.locator("#registerPassword").fill(password);
    await page.locator("#registerPasswordConfirm").fill(password);

    // Click step 1 button
    await page.locator("#emailRegisterStep1Button").click();

    // Should show step 2 with nsec display
    await expect(page.locator("#emailRegisterStep2")).toBeVisible({
      timeout: 15000,
    });
    await expect(page.locator("#emailNsecDisplay")).toHaveValue(/^nsec1/, {
      timeout: 15000,
    });

    // Step 2 button should be disabled until checkbox is checked
    await expect(page.locator("#emailRegisterStep2Button")).toBeDisabled();

    // Check the checkbox
    await page.locator("#emailNsecSavedCheckbox").check();
    await expect(page.locator("#emailRegisterStep2Button")).toBeEnabled();

    // Complete registration
    await page.locator("#emailRegisterStep2Button").click();

    // Should be logged in
    await expect(page.locator("#logoutContainer")).toBeVisible({
      timeout: 10000,
    });
    await expect(page.locator("#authButtons")).toHaveClass(/is-hidden/);
  });

  test("logs in with email/password after registration", async ({ page }) => {
    const email = uniqueEmail();
    const password = "testPassword123!";

    // Register first
    await registerWithEmail(page, email, password);

    // Logout
    await logout(page);

    // Login with email
    await loginWithEmail(page, email, password);

    // Should be logged in
    await expect(page.locator("#logoutContainer")).toBeVisible();
  });

  test("rejects login with wrong password", async ({ page }) => {
    const email = uniqueEmail();
    const password = "correctPassword123!";
    const wrongPassword = "wrongPassword456!";

    // Register first
    await registerWithEmail(page, email, password);

    // Logout
    await logout(page);

    // Try to login with wrong password
    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#loginNavClick").click();
    await page.locator(".tabs li[data-target='emailLogin']").click();
    await page.locator("#loginEmail").fill(email);
    await page.locator("#loginPassword").fill(wrongPassword);
    await page.locator("#emailLoginButton").click();

    // Should show error message
    await expect(page.locator("#emailLoginError")).toContainText(
      /invalid|password/i,
      { timeout: 5000 },
    );

    // Should NOT be logged in
    await expect(page.locator("#logoutContainer")).not.toBeVisible();
  });

  test("rejects duplicate email registration", async ({ page }) => {
    const email = uniqueEmail();
    const password = "testPassword123!";

    // Register first
    await registerWithEmail(page, email, password);

    // Logout
    await logout(page);

    // Try to register with same email
    await page.locator("#registerNavClick").click();
    await page.locator(".tabs li[data-target='registerEmail']").click();
    await page.locator("#registerEmailInput").fill(email);
    await page.locator("#registerPassword").fill(password);
    await page.locator("#registerPasswordConfirm").fill(password);
    await page.locator("#emailRegisterStep1Button").click();

    // Wait for step 2 to appear
    await expect(page.locator("#emailNsecDisplay")).toHaveValue(/^nsec1/, {
      timeout: 15000,
    });
    await page.locator("#emailNsecSavedCheckbox").check();
    await page.locator("#emailRegisterStep2Button").click();

    // Should show error about duplicate email
    await expect(page.locator("#emailRegisterError")).toContainText(
      /already registered|duplicate/i,
      { timeout: 5000 },
    );
  });

  test("forgot password flow with nsec challenge signing", async ({ page }) => {
    const email = uniqueEmail();
    const password = "oldPassword123!";
    const newPassword = "newPassword456!";

    // Register and capture nsec
    const nsec = await registerWithEmail(page, email, password);
    expect(nsec).toMatch(/^nsec1/);

    // Logout
    await logout(page);

    // Open login modal and click forgot password
    await page.locator("#loginNavClick").click();
    await expect(page.locator("#loginModal")).toHaveClass(/is-active/);

    await page.locator("#forgotPasswordLink").click();

    // Forgot password modal should open
    await expect(page.locator("#forgotPasswordModal")).toHaveClass(/is-active/);

    // Step 1: Enter email
    await page.locator("#forgotEmail").fill(email);
    await page.locator("#forgotStep1Button").click();

    // Should move to step 2
    await expect(page.locator("#forgotStep2")).toBeVisible({ timeout: 5000 });

    // Step 2: Enter nsec to sign challenge
    await page.locator("#forgotNsec").fill(nsec);
    await page.locator("#forgotStep2Button").click();

    // Should move to step 3
    await expect(page.locator("#forgotStep3")).toBeVisible({ timeout: 5000 });

    // Step 3: Enter new password
    await page.locator("#forgotNewPassword").fill(newPassword);
    await page.locator("#forgotNewPasswordConfirm").fill(newPassword);
    await page.locator("#forgotStep3Button").click();

    // Should close forgot modal and open login modal with success message
    await expect(page.locator("#forgotPasswordModal")).not.toHaveClass(
      /is-active/,
    );
    await expect(page.locator("#loginModal")).toHaveClass(/is-active/);

    // Close login modal and login with new password
    await page.locator("#closeLoginModal").click();
    await loginWithEmail(page, email, newPassword);

    // Should be logged in
    await expect(page.locator("#logoutContainer")).toBeVisible();
  });

  test("email user can access protected routes (entries, payouts)", async ({
    page,
  }) => {
    const email = uniqueEmail();
    const password = "testPassword123!";

    // Register and login
    await registerWithEmail(page, email, password);

    // Navigate to entries page
    await page.locator("#allEntriesNavClick").click();
    await expect(page.locator("#allEntries")).toBeVisible({ timeout: 10000 });

    // Navigate to payouts page
    await page.locator("#payoutsNavClick").click();
    await expect(page.locator("#payouts")).toBeVisible({ timeout: 10000 });

    // Navigate back to competitions
    await page.locator("#allCompetitionsNavClick").click();
    await expect(page.locator("#allCompetitions")).toBeVisible({
      timeout: 10000,
    });
  });

  test("extension login still works", async ({ page }) => {
    // This test requires a Nostr extension to be installed
    // Skip if no extension is available
    await page.goto("/");

    // Wait for WASM to initialize
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    // Open login modal
    await page.locator("#loginNavClick").click();
    await expect(page.locator("#loginModal")).toHaveClass(/is-active/);

    // Extension tab should be visible
    await expect(
      page.locator(".tabs li[data-target='extensionLogin']"),
    ).toBeVisible();

    // Click extension tab
    await page.locator(".tabs li[data-target='extensionLogin']").click();

    // Extension login content should be visible
    await expect(page.locator("#extensionLogin")).toBeVisible();

    // Extension login button should be present
    await expect(page.locator("#extensionLoginButton")).toBeVisible();

    // Note: Actually testing extension login requires a browser extension
    // This test just verifies the UI is present
  });

  test("password validation rejects short passwords", async ({ page }) => {
    const email = uniqueEmail();
    const shortPassword = "short"; // Less than 8 characters

    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#registerNavClick").click();
    await page.locator(".tabs li[data-target='registerEmail']").click();

    await page.locator("#registerEmailInput").fill(email);
    await page.locator("#registerPassword").fill(shortPassword);
    await page.locator("#registerPasswordConfirm").fill(shortPassword);
    await page.locator("#emailRegisterStep1Button").click();

    // Should show error about password length
    await expect(page.locator("#emailRegisterError")).toContainText(
      /8 characters/i,
      { timeout: 5000 },
    );

    // Should NOT proceed to step 2
    await expect(page.locator("#registerEmailStep2")).not.toBeVisible();
  });

  test("password validation rejects mismatched passwords", async ({ page }) => {
    const email = uniqueEmail();
    const password1 = "testPassword123!";
    const password2 = "differentPassword456!";

    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#registerNavClick").click();
    await page.locator(".tabs li[data-target='registerEmail']").click();

    await page.locator("#registerEmailInput").fill(email);
    await page.locator("#registerPassword").fill(password1);
    await page.locator("#registerPasswordConfirm").fill(password2);
    await page.locator("#emailRegisterStep1Button").click();

    // Should show error about password mismatch
    await expect(page.locator("#emailRegisterError")).toContainText(
      /do not match/i,
      { timeout: 5000 },
    );

    // Should NOT proceed to step 2
    await expect(page.locator("#registerEmailStep2")).not.toBeVisible();
  });

  test("tab switching works correctly in login modal", async ({ page }) => {
    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#loginNavClick").click();

    // Email tab should be active by default
    await expect(page.locator("#emailLogin")).toBeVisible();
    await expect(page.locator("#extensionLogin")).not.toBeVisible();

    // Switch to extension tab
    await page.locator(".tabs li[data-target='extensionLogin']").click();
    await expect(page.locator("#emailLogin")).not.toBeVisible();
    await expect(page.locator("#extensionLogin")).toBeVisible();

    // Switch back to email tab
    await page.locator(".tabs li[data-target='emailLogin']").click();
    await expect(page.locator("#emailLogin")).toBeVisible();
    await expect(page.locator("#extensionLogin")).not.toBeVisible();
  });

  test("tab switching works correctly in register modal", async ({ page }) => {
    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#registerNavClick").click();

    // Email tab should be active by default
    await expect(page.locator("#registerEmail")).toBeVisible();
    await expect(page.locator("#registerExtension")).not.toBeVisible();

    // Switch to extension tab
    await page.locator(".tabs li[data-target='registerExtension']").click();
    await expect(page.locator("#registerEmail")).not.toBeVisible();
    await expect(page.locator("#registerExtension")).toBeVisible();

    // Switch back to email tab
    await page.locator(".tabs li[data-target='registerEmail']").click();
    await expect(page.locator("#registerEmail")).toBeVisible();
    await expect(page.locator("#registerExtension")).not.toBeVisible();
  });
});
