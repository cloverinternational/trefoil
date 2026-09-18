import { defineConfig, devices } from '@playwright/test';

const port = 14773;
const chromium = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH
  ?? '/home/swarm/.cache/ms-playwright/chromium-1234/chrome-linux/chrome';

export default defineConfig({
  testDir: './e2e',
  testIgnore: ['../tests/**'],
  fullyParallel: true,
  retries: 0,
  reporter: 'list',
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    trace: 'retain-on-failure',
    launchOptions: { executablePath: chromium },
  },
  webServer: {
    command: `npm run preview -- --host 127.0.0.1 --port ${port}`,
    url: `http://127.0.0.1:${port}`,
    reuseExistingServer: false,
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
