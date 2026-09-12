import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./web-tests",
  timeout: 30_000,
  expect: { timeout: 7_500 },
  // Real decoder/10k-card workloads share this budget. The CLI can raise it
  // for measured stress runs; CPU count alone does not bound GPU setup costs.
  workers: 4,
  fullyParallel: true,
  forbidOnly: Boolean(process.env.CI),
  retries: 0,
  reporter: [
    ["line"],
    ...(process.env.CI ? [["html", { open: "never" }]] : []),
    ...(process.env.RUSTY_DLNA_BROWSER_EVIDENCE ? [["./scripts/browser-diagnostics-reporter.mjs"]] : []),
  ],
  use: {
    baseURL: "http://127.0.0.1:18201",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] } },
    { name: "mobile-chromium", use: { ...devices["Pixel 7"] } },
  ],
  webServer: {
    command: "scripts/playwright-server.sh",
    url: "http://127.0.0.1:18201/",
    reuseExistingServer: false,
    timeout: 120_000,
    gracefulShutdown: { signal: "SIGTERM", timeout: 5_000 },
    stdout: "pipe",
    stderr: "pipe",
  },
});
