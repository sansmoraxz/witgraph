import { defineConfig } from '@playwright/test';

/**
 * Chromium flags turning on WebGPU (on SwiftShader, so no GPU is needed)
 * and WebNN.
 */
const ACCELERATORS = [
  '--enable-unsafe-webgpu',
  '--use-angle=swiftshader',
  '--enable-features=Vulkan,WebMachineLearningNeuralNetwork',
  '--enable-experimental-web-platform-features',
];

export default defineConfig({
  testDir: 'test',
  timeout: 30_000,
  use: {
    baseURL: 'http://localhost:5173',
    headless: true,
    // The demo graph is laid out for a desktop window: at the default 1280x720
    // the timeline panel covers the lower nodes.
    viewport: { width: 1600, height: 1000 },
  },
  webServer: {
    command: 'npx vite --port 5173',
    port: 5173,
    reuseExistingServer: true,
    timeout: 15_000,
  },
  projects: [
    // Chromium as it ships: no WebGPU adapter, no WebNN.
    { name: 'chromium', use: { browserName: 'chromium' } },
    // With both: only the accelerator tests run here.
    {
      name: 'chromium-accelerated',
      testMatch: /accelerators\.spec\.ts/,
      use: { browserName: 'chromium', launchOptions: { args: ACCELERATORS } },
    },
  ],
});
