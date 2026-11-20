import { expect, Page } from "@playwright/test";

/**
 * Sends a username signup's step 2 and waits until the account is logged in.
 *
 * The welcome screen logs in after a two-second timer. The page's clock is
 * moved past it rather than waiting it out, which would cost every signup
 * in the suite two seconds.
 */
export async function finishUsernameSignup(page: Page): Promise<void> {
  await page.clock.install();
  await page.locator("#usernameRegisterStep2Button").click();
  await expect(page.locator("#usernameRegisterStep3")).toBeVisible({
    timeout: 10000,
  });
  await page.clock.fastForward(2000);
  await expect(page.locator("#logoutContainer")).toBeVisible({
    timeout: 10000,
  });
}
