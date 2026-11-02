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

/** A fake camera, and no asking whether it may be used. */
const FAKE_CAMERA = ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream'];

/**
 * The dev server's port. A server already on it is reused, whatever checkout it
 * serves: set this to test a worktree while another checkout's server runs.
 */
const PORT = Number(process.env.DEMO_PORT ?? 5173);

export default defineConfig({
  testDir: 'test',
  timeout: 30_000,
  use: {
    baseURL: `http://localhost:${PORT}`,
    headless: true,
    // The demo graph is laid out for a desktop window: at the default 1280x720
    // the timeline panel covers the lower nodes.
    viewport: { width: 1600, height: 1000 },
  },
  webServer: {
    command: `npx vite --port ${PORT}`,
    port: PORT,
    reuseExistingServer: true,
    timeout: 15_000,
  },
  projects: [
    // Chromium as it ships: no WebGPU adapter, no WebNN.
    {
      name: 'chromium',
      use: { browserName: 'chromium', launchOptions: { args: FAKE_CAMERA } },
    },
    // With both: only the accelerator, tensor and camera tests run here.
    {
      name: 'chromium-accelerated',
      testMatch: /(accelerators|tensors|camera)\.spec\.ts/,
      use: { browserName: 'chromium', launchOptions: { args: [...ACCELERATORS, ...FAKE_CAMERA] } },
    },
  ],
});
