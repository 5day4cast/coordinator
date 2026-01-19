import { test, expect, Page } from "@playwright/test";

function uniqueUsername(): string {
  return `user${Date.now()}${Math.random().toString(36).slice(2, 6)}`;
}

async function registerWithUsername(
  page: Page,
  username: string,
  password: string,
): Promise<string> {
  await page.goto("/");

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

  const nsec = await page.locator("#usernameNsecDisplay").inputValue();

  await page.locator("#usernameNsecSavedCheckbox").check();

  await page.locator("#usernameRegisterStep2Button").click();

  await expect(page.locator("#logoutContainer")).toBeVisible({
    timeout: 10000,
  });

  return nsec;
}

async function loginWithUsername(
  page: Page,
  username: string,
  password: string,
): Promise<void> {
  await page.goto("/");

  await page.waitForFunction(() => window.wasmInitialized === true, {
    timeout: 15000,
  });

  await page.locator("#loginNavClick").click();
  await expect(page.locator("#loginModal")).toHaveClass(/is-active/);

  await page.locator(".tabs li[data-target='usernameLogin']").click();

  await page.locator("#loginUsername").fill(username);
  await page.locator("#loginPassword").fill(password);

  await page.locator("#usernameLoginButton").click();

  await expect(page.locator("#logoutContainer")).toBeVisible({
    timeout: 10000,
  });
}

async function logout(page: Page): Promise<void> {
  await page.locator("#logoutNavClick").click();
  await expect(page.locator("#authButtons")).toBeVisible();
}

test.describe("Username/Password Authentication", () => {
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

  test("registers new user with username/password and shows nsec backup", async ({
    page,
  }) => {
    const username = uniqueUsername();
    const password = "testPassword123!";

    await page.goto("/");

    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#registerNavClick").click();
    await expect(page.locator("#registerModal")).toHaveClass(/is-active/);

    await expect(
      page.locator(".tabs li[data-target='registerUsername']"),
    ).toBeVisible();

    await page.locator("#registerUsernameInput").fill(username);
    await page.locator("#registerPassword").fill(password);
    await page.locator("#registerPasswordConfirm").fill(password);

    await page.locator("#usernameRegisterStep1Button").click();

    await expect(page.locator("#usernameRegisterStep2")).toBeVisible({
      timeout: 15000,
    });
    await expect(page.locator("#usernameNsecDisplay")).toHaveValue(/^nsec1/, {
      timeout: 15000,
    });

    await expect(page.locator("#usernameRegisterStep2Button")).toBeDisabled();

    await page.locator("#usernameNsecSavedCheckbox").check();
    await expect(page.locator("#usernameRegisterStep2Button")).toBeEnabled();

    await page.locator("#usernameRegisterStep2Button").click();

    await expect(page.locator("#logoutContainer")).toBeVisible({
      timeout: 10000,
    });
    await expect(page.locator("#authButtons")).toHaveClass(/is-hidden/);
  });

  test("logs in with username/password after registration", async ({
    page,
  }) => {
    const username = uniqueUsername();
    const password = "testPassword123!";

    await registerWithUsername(page, username, password);

    await logout(page);

    await loginWithUsername(page, username, password);

    await expect(page.locator("#logoutContainer")).toBeVisible();
  });

  test("rejects login with wrong password", async ({ page }) => {
    const username = uniqueUsername();
    const password = "correctPassword123!";
    const wrongPassword = "wrongPassword456!";

    await registerWithUsername(page, username, password);

    await logout(page);

    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#loginNavClick").click();
    await page.locator(".tabs li[data-target='usernameLogin']").click();
    await page.locator("#loginUsername").fill(username);
    await page.locator("#loginPassword").fill(wrongPassword);
    await page.locator("#usernameLoginButton").click();

    await expect(page.locator("#usernameLoginError")).toContainText(
      /invalid|password/i,
      { timeout: 5000 },
    );

    await expect(page.locator("#logoutContainer")).not.toBeVisible();
  });

  test("duplicate username registration silently fails to prevent enumeration", async ({
    page,
  }) => {
    const username = uniqueUsername();
    const password = "testPassword123!";
    const differentPassword = "DifferentPass456!";

    await registerWithUsername(page, username, password);

    await logout(page);

    await page.locator("#registerNavClick").click();
    await page.locator(".tabs li[data-target='registerUsername']").click();
    await page.locator("#registerUsernameInput").fill(username);
    await page.locator("#registerPassword").fill(differentPassword);
    await page.locator("#registerPasswordConfirm").fill(differentPassword);
    await page.locator("#usernameRegisterStep1Button").click();

    await expect(page.locator("#usernameNsecDisplay")).toHaveValue(/^nsec1/, {
      timeout: 15000,
    });
    await page.locator("#usernameNsecSavedCheckbox").check();
    await page.locator("#usernameRegisterStep2Button").click();

    await expect(page.locator("#usernameRegisterStep3")).toBeVisible({
      timeout: 10000,
    });

    await page.locator("#closeResisterModal").click();

    await page.locator("#loginNavClick").click();
    await page.locator(".tabs li[data-target='usernameLogin']").click();
    await page.locator("#loginUsername").fill(username);
    await page.locator("#loginPassword").fill(differentPassword);
    await page.locator("#usernameLoginButton").click();

    await expect(page.locator("#usernameLoginError")).toContainText(
      /invalid|failed|password/i,
      { timeout: 10000 },
    );

    await page.locator("#loginPassword").fill(password);
    await page.locator("#usernameLoginButton").click();
    await expect(page.locator("#logoutContainer")).toBeVisible({
      timeout: 10000,
    });
  });

  test("forgot password flow with nsec challenge signing", async ({ page }) => {
    const username = uniqueUsername();
    const password = "oldPassword123!";
    const newPassword = "newPassword456!";

    const nsec = await registerWithUsername(page, username, password);
    expect(nsec).toMatch(/^nsec1/);

    await logout(page);

    await page.locator("#loginNavClick").click();
    await expect(page.locator("#loginModal")).toHaveClass(/is-active/);

    await page.locator("#forgotPasswordLink").click();

    await expect(page.locator("#forgotPasswordModal")).toHaveClass(/is-active/);

    await page.locator("#forgotUsername").fill(username);
    await page.locator("#forgotStep1Button").click();

    await expect(page.locator("#forgotStep2")).toBeVisible({ timeout: 5000 });

    await page.locator("#forgotNsec").fill(nsec);
    await page.locator("#forgotStep2Button").click();

    await expect(page.locator("#forgotStep3")).toBeVisible({ timeout: 5000 });

    await page.locator("#forgotNewPassword").fill(newPassword);
    await page.locator("#forgotNewPasswordConfirm").fill(newPassword);
    await page.locator("#forgotStep3Button").click();

    await expect(page.locator("#forgotPasswordModal")).not.toHaveClass(
      /is-active/,
    );
    await expect(page.locator("#loginModal")).toHaveClass(/is-active/);

    await page.locator("#closeLoginModal").click();
    await loginWithUsername(page, username, newPassword);

    await expect(page.locator("#logoutContainer")).toBeVisible();
  });

  test("username user can access protected routes (entries, payouts)", async ({
    page,
  }) => {
    const username = uniqueUsername();
    const password = "testPassword123!";

    await registerWithUsername(page, username, password);

    await page.locator("#allEntriesNavClick").click();
    await expect(page.locator("#allEntries")).toBeVisible({ timeout: 10000 });

    await page.locator("#payoutsNavClick").click();
    await expect(page.locator("#payouts")).toBeVisible({ timeout: 10000 });

    await page.locator("#allCompetitionsNavClick").click();
    await expect(page.locator("#allCompetitions")).toBeVisible({
      timeout: 10000,
    });
  });

  test("extension login still works", async ({ page }) => {
    await page.goto("/");

    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#loginNavClick").click();
    await expect(page.locator("#loginModal")).toHaveClass(/is-active/);

    await expect(
      page.locator(".tabs li[data-target='extensionLogin']"),
    ).toBeVisible();

    await page.locator(".tabs li[data-target='extensionLogin']").click();

    await expect(page.locator("#extensionLogin")).toBeVisible();

    await expect(page.locator("#extensionLoginButton")).toBeVisible();
  });

  test("username-registered user can login with same nsec (extension upgrade)", async ({
    page,
  }) => {
    const username = uniqueUsername();
    const password = "testPassword123!";

    const nsec = await registerWithUsername(page, username, password);
    expect(nsec).toMatch(/^nsec1/);

    await logout(page);

    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    const loginSuccess = await page.evaluate(async (nsec) => {
      try {
        await window.nostrClient.initialize(window.SignerType.PrivateKey, nsec);

        const apiBase = document.body.dataset.apiBase;
        const authorizedClient = new window.AuthorizedClient(
          window.nostrClient,
          apiBase,
        );

        const response = await authorizedClient.post(
          `${apiBase}/api/v1/users/login`,
        );

        if (!response.ok) {
          return { success: false, status: response.status };
        }

        const data = await response.json();
        return {
          success: true,
          hasEncryptedKey: !!data.encrypted_bitcoin_private_key,
          hasNetwork: !!data.network,
        };
      } catch (error) {
        return { success: false, error: error.message };
      }
    }, nsec);

    expect(loginSuccess.success).toBe(true);
    expect(loginSuccess.hasEncryptedKey).toBe(true);
    expect(loginSuccess.hasNetwork).toBe(true);
  });

  test("password validation rejects weak passwords", async ({ page }) => {
    const username = uniqueUsername();
    const weakPassword = "short";

    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#registerNavClick").click();
    await page.locator(".tabs li[data-target='registerUsername']").click();

    await page.locator("#registerUsernameInput").fill(username);
    await page.locator("#registerPassword").fill(weakPassword);
    await page.locator("#registerPasswordConfirm").fill(weakPassword);
    await page.locator("#usernameRegisterStep1Button").click();

    await expect(page.locator("#usernameRegisterError")).toContainText(
      /10 characters/i,
      { timeout: 5000 },
    );

    await expect(page.locator("#usernameRegisterStep2")).not.toBeVisible();
  });

  test("password validation rejects mismatched passwords", async ({ page }) => {
    const username = uniqueUsername();
    const password1 = "testPassword123!";
    const password2 = "differentPassword456!";

    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#registerNavClick").click();
    await page.locator(".tabs li[data-target='registerUsername']").click();

    await page.locator("#registerUsernameInput").fill(username);
    await page.locator("#registerPassword").fill(password1);
    await page.locator("#registerPasswordConfirm").fill(password2);
    await page.locator("#usernameRegisterStep1Button").click();

    await expect(page.locator("#usernameRegisterError")).toContainText(
      /do not match/i,
      { timeout: 5000 },
    );

    await expect(page.locator("#usernameRegisterStep2")).not.toBeVisible();
  });

  test("username validation rejects invalid usernames", async ({ page }) => {
    const password = "testPassword123!";

    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#registerNavClick").click();
    await page.locator(".tabs li[data-target='registerUsername']").click();

    // Test too short
    await page.locator("#registerUsernameInput").fill("ab");
    await page.locator("#registerPassword").fill(password);
    await page.locator("#registerPasswordConfirm").fill(password);
    await page.locator("#usernameRegisterStep1Button").click();

    await expect(page.locator("#usernameRegisterError")).toContainText(
      /at least 3 characters/i,
      { timeout: 5000 },
    );

    // Test starts with number
    await page.locator("#registerUsernameInput").fill("123user");
    await page.locator("#usernameRegisterStep1Button").click();

    await expect(page.locator("#usernameRegisterError")).toContainText(
      /start with a letter/i,
      { timeout: 5000 },
    );
  });

  test("tab switching works correctly in login modal", async ({ page }) => {
    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#loginNavClick").click();

    await expect(page.locator("#usernameLogin")).toBeVisible();
    await expect(page.locator("#extensionLogin")).not.toBeVisible();

    await page.locator(".tabs li[data-target='extensionLogin']").click();
    await expect(page.locator("#usernameLogin")).not.toBeVisible();
    await expect(page.locator("#extensionLogin")).toBeVisible();

    await page.locator(".tabs li[data-target='usernameLogin']").click();
    await expect(page.locator("#usernameLogin")).toBeVisible();
    await expect(page.locator("#extensionLogin")).not.toBeVisible();
  });

  test("tab switching works correctly in register modal", async ({ page }) => {
    await page.goto("/");
    await page.waitForFunction(() => window.wasmInitialized === true, {
      timeout: 15000,
    });

    await page.locator("#registerNavClick").click();

    await expect(page.locator("#registerUsername")).toBeVisible();
    await expect(page.locator("#registerExtension")).not.toBeVisible();

    await page.locator(".tabs li[data-target='registerExtension']").click();
    await expect(page.locator("#registerUsername")).not.toBeVisible();
    await expect(page.locator("#registerExtension")).toBeVisible();

    await page.locator(".tabs li[data-target='registerUsername']").click();
    await expect(page.locator("#registerUsername")).toBeVisible();
    await expect(page.locator("#registerExtension")).not.toBeVisible();
  });
});
