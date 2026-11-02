import { defineConfig } from '@playwright/test';

/** A fake camera as fast as frames can be asked for, and no asking whether it may be used. */
const CAMERA = ['--use-fake-device-for-media-stream=fps=60', '--use-fake-ui-for-media-stream'];

export default defineConfig({
  testDir: 'bench',
  timeout: 25 * 60_000,
  use: { baseURL: 'http://localhost:5174', headless: true },
  webServer: {
    command: 'npx vite --port 5174',
    port: 5174,
    reuseExistingServer: true,
    timeout: 15_000,
  },
  projects: [
    {
      name: 'swiftshader',
      use: {
        browserName: 'chromium',
        launchOptions: {
          args: [
            '--enable-unsafe-webgpu',
            '--use-angle=swiftshader',
            '--enable-features=Vulkan,WebMachineLearningNeuralNetwork',
            '--enable-experimental-web-platform-features',
            ...CAMERA,
          ],
        },
      },
    },
    {
      // The real GPU: headless Chromium only finds SwiftShader here.
      name: 'gpu',
      use: {
        browserName: 'chromium',
        headless: false,
        launchOptions: {
          args: [
            '--enable-unsafe-webgpu',
            '--ignore-gpu-blocklist',
            '--ozone-platform=x11',
            '--enable-features=Vulkan,WebMachineLearningNeuralNetwork',
            '--enable-experimental-web-platform-features',
            ...CAMERA,
          ],
        },
      },
    },
  ],
});
