import { expect, test } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

const adminURL = process.env.COORDINATOR_ADMIN_URL || "http://localhost:9991";
const adminToken = (
  process.env.COORDINATOR_ADMIN_TOKEN ||
  readFileSync(join(__dirname, "..", "..", "config", "e2e_admin_token"), "utf8")
).trim();

test("operator browser session authenticates and enforces CSRF", async ({ page, request }) => {
  const publicLogin = await request.get("/admin/login");
  expect(publicLogin.status()).toBe(404);
  const publicWallet = await request.get("/api/v1/wallet/balance", {
    headers: { Authorization: `Bearer ${adminToken}` },
  });
  expect(publicWallet.status()).toBe(404);

  await page.goto(`${adminURL}/admin/login`);
  await page.getByLabel("Admin token").fill(adminToken);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page).toHaveURL(`${adminURL}/admin`);
  await page.goto(`${adminURL}/admin/wallet`);
  const csrfHeaders = await page.locator("body").getAttribute("hx-headers");
  expect(csrfHeaders).not.toBeNull();

  // Browser fetch supplies the real cookie. The invalid ID prevents any mutation.
  const statuses = await page.evaluate(async (headers: Record<string, string>) => {
    const url = "/admin/api/competitions/delete";
    const body = "competition_id=not-a-uuid";
    const withoutCsrf = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body,
    });
    const withCsrf = await fetch(url, {
      method: "POST",
      headers: { ...headers, "Content-Type": "application/x-www-form-urlencoded" },
      body,
    });
    return {
      rejected: withoutCsrf.status,
      accepted: withCsrf.status,
      body: await withCsrf.text(),
    };
  }, JSON.parse(csrfHeaders!));
  expect(statuses.rejected).toBe(403);
  expect(statuses.accepted).toBe(200);
  expect(statuses.body).toContain("Invalid competition ID");
});
