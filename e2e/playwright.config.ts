import { defineConfig, devices } from "@playwright/test";

/**
 * Playwright configuration for coordinator E2E tests.
 *
 * Prerequisites:
 * - Coordinator server running on localhost:3000
 * - Bitcoin regtest running
 * - LND nodes running with channels set up
 * - Keymeld gateway + enclaves running
 *
 * Use `just start` to start all services before running tests.
 */
export default defineConfig({
  testDir: "./tests",
  fullyParallel: false, // Run tests sequentially for blockchain state consistency
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: 1, // Single worker for stateful blockchain tests
  reporter: [["html", { open: "never" }], ["list"]],

  use: {
    baseURL: process.env.COORDINATOR_URL || "http://localhost:9990",
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    video: "retain-on-failure",
  },

  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],

  // Global timeout for each test
  timeout: 60_000,

  // Expect timeout for assertions
  expect: {
    timeout: 10_000,
  },
});
