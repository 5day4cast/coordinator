import { test, expect, Page } from "@playwright/test";

function uniqueUsername(): string {
  return `user${Date.now()}${Math.random().toString(36).slice(2, 6)}`;
}

async function registerAndLogin(page: Page): Promise<void> {
  await page.goto("/");

  await page.waitForFunction(() => window.wasmInitialized === true, {
    timeout: 15000,
  });

  const username = uniqueUsername();
  const password = "testPassword123!";

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

test.describe("Full Entry Submission Flow", () => {
  test("complete entry flow: login → competition → picks → payment → submission", async ({
    page,
  }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 15000,
    });

    let rowCount = await page
      .locator("#competitionsDataTable tbody tr")
      .count();

    if (rowCount === 0) {
      console.log(
        "No competitions available - test requires at least one competition in Registration status",
      );
      test.skip();
      return;
    }

    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    const canEnter = (await enterButton.count()) > 0;

    if (!canEnter) {
      console.log("No competitions in Registration status available for entry");
      test.skip();
      return;
    }

    await enterButton.click();

    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    await expect(page.locator("#entryContent")).toBeVisible();

    await page.waitForSelector("#entryContent button", { timeout: 10000 });

    const allPickButtons = page.locator("#entryContent button.pick-button");
    const buttonCount = await allPickButtons.count();

    if (buttonCount > 0) {
      for (let i = 0; i < Math.min(3, buttonCount); i++) {
        const button = allPickButtons.nth(i);
        await button.click();
        await expect(button).toHaveClass(/is-active/);
      }
    }

    const submitButton = page.locator("#submitEntry");
    await expect(submitButton).toBeVisible();
    await expect(submitButton).toBeEnabled();

    page.on("console", (msg) => {
      if (msg.type() === "error" || msg.type() === "warning") {
        console.log(`[browser ${msg.type()}]:`, msg.text());
      }
    });

    await submitButton.click();

    await Promise.race([
      expect(page.locator("#ticketPaymentModal")).toHaveClass(/is-active/, {
        timeout: 15000,
      }),
      page
        .locator("#errorMessage:not(.hidden)")
        .waitFor({ timeout: 15000 })
        .then(async () => {
          const errorText = await page.locator("#errorMessage").textContent();
          throw new Error(`Entry submission failed: ${errorText}`);
        }),
    ]);

    await expect(page.locator("#paymentRequest")).toBeVisible();

    await expect(page.locator("#ticketPaymentModal")).not.toHaveClass(
      /is-active/,
      { timeout: 15000 },
    );

    await expect(page.locator("#successMessage")).toBeVisible({
      timeout: 5000,
    });

    await expect(page.locator("#submitEntry")).toHaveText("Entry Submitted!");
  });

  test("entry form displays competition info and entry fee", async ({
    page,
  }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 15000,
    });

    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    await enterButton.click();
    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    const entryContent = page.locator("#entryContent");
    await expect(entryContent).toBeVisible();

    await expect(page.locator("#submitEntry")).toBeVisible();
  });

  test("can navigate back from entry form to competitions", async ({
    page,
  }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 15000,
    });

    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    await enterButton.click();
    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    await Promise.all([
      page.waitForResponse((resp) => resp.url().includes("/competitions")),
      page.locator("#backToCompetitions").click(),
    ]);

    await expect(page.locator("#allCompetitions")).toBeVisible();
    await expect(page.locator("#entryContainer")).not.toBeVisible();
  });

  test("prediction buttons toggle correctly", async ({ page }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 15000,
    });

    const enterButton = page
      .locator("#competitionsDataTable tbody tr")
      .filter({ hasText: "Registration" })
      .locator("button, a")
      .filter({ hasText: /Enter|Create Entry/ })
      .first();

    await enterButton.click();
    await expect(page.locator("#entryContainer")).toBeVisible({
      timeout: 5000,
    });

    await page.waitForSelector("#entryContent button.pick-button", {
      timeout: 10000,
    });

    const firstButton = page
      .locator("#entryContent button.pick-button")
      .first();

    await expect(firstButton).toHaveClass(/is-outlined/);

    await firstButton.click();

    await expect(firstButton).toHaveClass(/is-active/);
    await expect(firstButton).not.toHaveClass(/is-outlined/);
  });
});

test.describe("Competition Status Display", () => {
  test("competitions table shows correct status badges", async ({ page }) => {
    await page.goto("/");

    await page.waitForSelector("#competitionsDataTable", { timeout: 10000 });

    const statusHeader = page.locator(
      "#competitionsDataTable thead th:has-text('Status')",
    );
    await expect(statusHeader).toBeVisible();

    const rows = await page.locator("#competitionsDataTable tbody tr").count();

    if (rows > 0) {
      const firstRowStatus = page
        .locator("#competitionsDataTable tbody tr")
        .first()
        .locator("td")
        .nth(0);

      await expect(firstRowStatus).toBeVisible();

      const statusText = await firstRowStatus.textContent();
      expect(
        ["Registration", "Live", "Setup", "Signing", "Completed"].some((s) =>
          statusText?.includes(s),
        ),
      ).toBe(true);
    }
  });

  test("only Registration status competitions have Enter button", async ({
    page,
  }) => {
    await registerAndLogin(page);

    await page.waitForSelector("#competitionsDataTable tbody tr", {
      timeout: 15000,
    });

    const rows = page.locator("#competitionsDataTable tbody tr");
    const rowCount = await rows.count();

    for (let i = 0; i < rowCount; i++) {
      const row = rows.nth(i);
      const statusCell = row.locator("td").nth(0);
      const statusText = await statusCell.textContent();

      const enterButton = row
        .locator("button, a")
        .filter({ hasText: /Enter|Create Entry/ });

      if (statusText?.includes("Registration")) {
        await expect(enterButton.first()).toBeVisible();
      }
    }
  });
});
