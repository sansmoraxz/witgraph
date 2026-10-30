import { test, expect } from '@playwright/test';

test.describe('WIT Graph Editor Demo', () => {
  test.beforeEach(async ({ page }) => {
    await page.goto('/');
    await page.waitForFunction(
      () => {
        const el = document.getElementById('status');
        return el?.classList.contains('ready') || el?.classList.contains('error');
      },
      { timeout: 30_000 },
    );
  });

  test('demo loads and graph compiles', async ({ page }) => {
    const status = page.locator('#status');
    await expect(status).toHaveClass(/ready/);
    await expect(status).toContainText('Graph compiles');
  });

  test('all web components are rendered', async ({ page }) => {
    await expect(page.locator('witgraph-controls')).toBeAttached();
    await expect(page.locator('witgraph-canvas')).toBeAttached();
    await expect(page.locator('witgraph-inspector')).toBeAttached();
    await expect(page.locator('witgraph-timeline')).toBeAttached();
    await expect(page.locator('witgraph-catalog')).toBeAttached();
  });

  test('canvas renders 8 nodes and 6 edges', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    await expect(canvas.locator('svg .node-group')).toHaveCount(8);
    await expect(canvas.locator('svg path.edge')).toHaveCount(6);
  });

  test('relay-a is pre-selected in inspector', async ({ page }) => {
    const inspector = page.locator('witgraph-inspector');
    await expect(inspector.locator('.node-id')).toHaveValue('relay-a');
  });

  test('clicking a node selects it', async ({ page }) => {
    const canvas = page.locator('witgraph-canvas');
    await canvas.locator('[data-node-id="relay-b"]').click({ timeout: 10_000 });
    await expect(page.locator('witgraph-inspector').locator('.node-id')).toHaveValue('relay-b');
  });

  test('controls toolbar buttons exist', async ({ page }) => {
    const controls = page.locator('witgraph-controls');
    await expect(controls.locator('button.run')).toBeAttached();
    await expect(controls.locator('button.step')).toBeAttached();
    await expect(controls.locator('button.stop')).toBeAttached();
    await expect(controls.locator('.badge')).toBeAttached();
  });

  test('connected port shows source in dropdown', async ({ page }) => {
    await page.locator('witgraph-canvas').locator('[data-node-id="relay-b"]').click();
    const selected = await page.locator('witgraph-inspector').evaluate((el: HTMLElement) => {
      const select = el.shadowRoot?.querySelector('select') as HTMLSelectElement | null;
      return select?.value ?? '';
    });
    expect(selected).toContain('relay-a');
  });

  test('auto-layout produces distinct positions', async ({ page }) => {
    const groups = page.locator('witgraph-canvas').locator('.node-group');
    const count = await groups.count();
    expect(count).toBe(8);
    const transforms = new Set<string>();
    for (let i = 0; i < count; i++) {
      transforms.add(await groups.nth(i).getAttribute('transform') ?? '');
    }
    expect(transforms.size).toBe(8);
  });

  test('catalog shows components', async ({ page }) => {
    const text = await page.locator('witgraph-catalog').evaluate(
      (el: HTMLElement) => el.shadowRoot?.textContent ?? '',
    );
    expect(text).toContain('Components');
  });

  test('injected values from attrs are visible', async ({ page }) => {
    const inspector = page.locator('witgraph-inspector');
    const text = await inspector.evaluate((el: HTMLElement) => el.shadowRoot?.textContent ?? '');
    expect(text).toContain('Injected');
  });
});
