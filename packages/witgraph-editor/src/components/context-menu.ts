export interface MenuItem {
  label: string;
  shortcut?: string;
  disabled?: boolean;
  separator?: boolean;
  action: () => void;
}

const MENU_CSS = `
  .ctx-menu {
    position: absolute; z-index: 100; min-width: 170px;
    background: #313244; border: 1px solid #45475a; border-radius: 6px;
    box-shadow: 0 4px 16px rgba(0,0,0,0.5); padding: 4px 0;
    font-family: system-ui, -apple-system, sans-serif; font-size: 12px;
    color: #cdd6f4;
  }
  .ctx-item {
    display: flex; justify-content: space-between; align-items: center;
    padding: 5px 12px; cursor: pointer; gap: 16px;
  }
  .ctx-item:hover { background: #45475a; }
  .ctx-item.disabled { opacity: 0.4; cursor: not-allowed; }
  .ctx-item.disabled:hover { background: transparent; }
  .ctx-shortcut { color: #6c7086; font-size: 11px; }
  .ctx-sep { height: 1px; background: #45475a; margin: 4px 8px; }
`;

export class ContextMenu {
  #el: HTMLDivElement | null = null;
  #styleEl: HTMLStyleElement | null = null;
  #dismissHandler = (e: Event) => {
    if (this.#el && !(e as PointerEvent).composedPath().includes(this.#el)) this.hide();
  };
  #escHandler = (e: Event) => {
    if ((e as KeyboardEvent).key === 'Escape') this.hide();
  };

  show(x: number, y: number, items: MenuItem[], container: ShadowRoot | HTMLElement): void {
    this.hide();

    if (!this.#styleEl) {
      this.#styleEl = document.createElement('style');
      this.#styleEl.textContent = MENU_CSS;
    }
    if (!this.#styleEl.parentNode) container.appendChild(this.#styleEl);

    const menu = document.createElement('div');
    menu.className = 'ctx-menu';
    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;

    for (const item of items) {
      if (item.separator) {
        const sep = document.createElement('div');
        sep.className = 'ctx-sep';
        menu.appendChild(sep);
        continue;
      }
      const row = document.createElement('div');
      row.className = item.disabled ? 'ctx-item disabled' : 'ctx-item';

      const label = document.createElement('span');
      label.textContent = item.label;
      row.appendChild(label);

      if (item.shortcut) {
        const sc = document.createElement('span');
        sc.className = 'ctx-shortcut';
        sc.textContent = item.shortcut;
        row.appendChild(sc);
      }

      if (!item.disabled) {
        row.addEventListener('click', (e) => {
          e.stopPropagation();
          this.hide();
          item.action();
        });
      }
      menu.appendChild(row);
    }

    container.appendChild(menu);
    this.#el = menu;

    requestAnimationFrame(() => {
      document.addEventListener('pointerdown', this.#dismissHandler, true);
      document.addEventListener('keydown', this.#escHandler, true);
    });
  }

  hide(): void {
    this.#el?.remove();
    this.#el = null;
    document.removeEventListener('pointerdown', this.#dismissHandler, true);
    document.removeEventListener('keydown', this.#escHandler, true);
  }
}
