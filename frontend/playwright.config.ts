import { defineConfig, devices } from '@playwright/test';

const port = 14773;
const liveBaseURL = process.env.TREFOIL_E2E_BASE_URL;
const chromium = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH;

export default defineConfig({
  testDir: './e2e',
  testIgnore: ['../tests/**'],
  fullyParallel: true,
  retries: 0,
  reporter: 'list',
  use: {
    baseURL: liveBaseURL || `http://127.0.0.1:${port}`,
    trace: 'retain-on-failure',
    launchOptions: chromium ? { executablePath: chromium } : {},
  },
  webServer: liveBaseURL ? undefined : {
    command: `npm run preview -- --host 127.0.0.1 --port ${port}`,
    url: `http://127.0.0.1:${port}`,
    reuseExistingServer: false,
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
