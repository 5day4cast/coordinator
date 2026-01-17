import { defineConfig, devices } from "@playwright/test";

/**
 * Playwright configuration for coordinator E2E tests.
 *
 * Prerequisites:
 * - Coordinator server running on localhost:9990
 *
 * For local development, use `just run` to start the coordinator.
 * In CI, the webServer config below will start the pre-built binary.
 */

const isCI = !!process.env.CI;

export default defineConfig({
  testDir: "./tests",
  globalSetup: "./global-setup.ts",
  fullyParallel: false, // Run tests sequentially for blockchain state consistency
  forbidOnly: isCI,
  retries: isCI ? 2 : 0,
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

  // Web server configuration
  // In CI, start the pre-built coordinator binary with e2e config
  // Locally, assume the server is already running (use `just run`)
  webServer: isCI
    ? {
        command:
          process.env.COORDINATOR_BIN ||
          "./target/release/coordinator --config ./config/e2e.toml",
        cwd: "..", // Run from project root so paths resolve correctly
        url: "http://localhost:9990",
        reuseExistingServer: false,
        timeout: 30_000, // 30 seconds - binary is pre-built, just needs to start
        stdout: "pipe",
        stderr: "pipe",
      }
    : undefined,
});
