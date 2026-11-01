import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: 'test',
  timeout: 30_000,
  use: {
    baseURL: 'http://localhost:5173',
    headless: true,
  },
  webServer: {
    command: 'npx vite --port 5173',
    port: 5173,
    reuseExistingServer: true,
    timeout: 15_000,
  },
  projects: [
    { name: 'chromium', use: { browserName: 'chromium' } },
  ],
});
