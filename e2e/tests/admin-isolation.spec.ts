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
  await expect(page).toHaveURL(`${adminURL}/admin/operations`);
  await page.goto(`${adminURL}/admin/wallet`);
  const csrfHeaders = await page.locator("body").getAttribute("hx-headers:inherited");
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

test.describe("native operator investigations", () => {
  test.use({ javaScriptEnabled: false });
  test("sorts operations and opens dependency checks without JavaScript", async ({ page, request }) => {
    for (const path of ["/admin/services", "/admin/keymeld"]) {
      expect((await request.get(path)).status()).toBe(404);
    }
    await page.goto(`${adminURL}/admin/login`);
    await page.getByLabel("Admin token").fill(adminToken);
    await page.getByRole("button", { name: "Sign in" }).click();
    await page.goto(`${adminURL}/admin/operations`);
    await expect(page.getByLabel("Sort by")).toHaveValue("created_desc");
    for (const sort of ["created_desc", "created_asc"]) {
      await page.getByLabel("Sort by").selectOption(sort);
      await page.locator('form[action="/admin/operations"] button[type="submit"]').click();
      await expect(page).toHaveURL(new RegExp(`sort=${sort}`));
      const times = await page.locator("table time[datetime]").evaluateAll(nodes =>
        nodes.map(n => Date.parse(n.getAttribute("datetime")!)));
      expect(times.length).toBeGreaterThan(0);
      expect(times.every(Number.isFinite)).toBeTruthy();
      expect(times).toEqual([...times].sort((a, b) => sort === "created_desc" ? b - a : a - b));
    }
    for (const show of ["finished", "paid_out"]) {
      await page.locator('select[name="show"]').selectOption(show);
      await page.locator('form[action="/admin/operations"] button[type="submit"]').click();
      await expect(page).toHaveURL(new RegExp(`show=${show}`));
      await expect(page.locator('select[name="show"]')).toHaveValue(show);
      await expect(page.getByLabel("Sort by")).toHaveValue("created_asc");
    }
    await page.getByRole("link", { name: "Clear filters", exact: true }).click();
    await expect(page.locator('select[name="show"]')).toHaveValue("all");
    await expect(page.getByLabel("Sort by")).toHaveValue("created_desc");
    for (const [path, title] of [["services", "Services"], ["keymeld", "Keymeld & enclaves"]]) {
      await page.goto(`${adminURL}/admin/${path}`);
      await expect(page.getByRole("heading", { name: title, exact: true })).toBeVisible();
      await expect(page.getByText("Grafana data is unavailable or not configured.", { exact: false })).toBeVisible();
      await expect(page.getByText("Metric query", { exact: true })).toHaveCount(0);
      await expect(page.getByRole("link", { name: "Refresh values", exact: true })).toBeVisible();
      const detail = page.locator("article.service-signal details").first();
      await detail.locator("summary").first().click();
      await expect(detail).toHaveAttribute("open", "");
      await page.setViewportSize({ width: 390, height: 844 });
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBeTruthy();
    }
  });
});
