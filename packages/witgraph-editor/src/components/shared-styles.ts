let _sheet: CSSStyleSheet | null = null;

export function getSharedSheet(): CSSStyleSheet {
  if (_sheet) return _sheet;
  _sheet = new CSSStyleSheet();
  _sheet.replaceSync(DEFAULT_CSS);
  return _sheet;
}

export function adoptStyles(shadow: ShadowRoot, extra?: string): void {
  const sheets = [getSharedSheet()];
  if (extra) {
    const local = new CSSStyleSheet();
    local.replaceSync(extra);
    sheets.push(local);
  }
  shadow.adoptedStyleSheets = sheets;
}

const DEFAULT_CSS = `
  /* Tailwind-inspired utility classes for shadow DOM components */
  *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }

  /* Layout */
  .block { display: block; }
  .flex { display: flex; }
  .grid { display: grid; }
  .inline-block { display: inline-block; }
  .inline-flex { display: inline-flex; }
  .hidden { display: none; }
  .items-center { align-items: center; }
  .justify-between { justify-content: space-between; }
  .flex-col { flex-direction: column; }
  .flex-wrap { flex-wrap: wrap; }
  .flex-1 { flex: 1 1 0%; }
  .gap-1 { gap: 0.25rem; }
  .gap-1\\.5 { gap: 0.375rem; }
  .gap-2 { gap: 0.5rem; }
  .gap-3 { gap: 0.75rem; }

  /* Spacing */
  .p-1 { padding: 0.25rem; }
  .p-2 { padding: 0.5rem; }
  .p-3 { padding: 0.75rem; }
  .px-2 { padding-left: 0.5rem; padding-right: 0.5rem; }
  .px-3 { padding-left: 0.75rem; padding-right: 0.75rem; }
  .py-1 { padding-top: 0.25rem; padding-bottom: 0.25rem; }
  .py-1\\.5 { padding-top: 0.375rem; padding-bottom: 0.375rem; }
  .py-2 { padding-top: 0.5rem; padding-bottom: 0.5rem; }
  .mb-1 { margin-bottom: 0.25rem; }
  .mb-2 { margin-bottom: 0.5rem; }
  .mb-3 { margin-bottom: 0.75rem; }
  .ml-1 { margin-left: 0.25rem; }
  .mt-1 { margin-top: 0.25rem; }
  .mt-2 { margin-top: 0.5rem; }

  /* Sizing */
  .w-full { width: 100%; }
  .h-full { height: 100%; }
  .min-h-0 { min-height: 0; }

  /* Overflow */
  .overflow-hidden { overflow: hidden; }
  .overflow-y-auto { overflow-y: auto; }
  .overflow-x-auto { overflow-x: auto; }

  /* Text */
  .text-xs { font-size: 0.6875rem; line-height: 1rem; }
  .text-sm { font-size: 0.75rem; line-height: 1rem; }
  .text-base { font-size: 0.875rem; line-height: 1.25rem; }
  .font-semibold { font-weight: 600; }
  .font-bold { font-weight: 700; }
  .font-mono { font-family: ui-monospace, 'Cascadia Code', 'Fira Code', monospace; }
  .font-sans { font-family: system-ui, -apple-system, sans-serif; }
  .text-center { text-align: center; }
  .uppercase { text-transform: uppercase; }
  .underline { text-decoration: underline; }
  .whitespace-pre-wrap { white-space: pre-wrap; }
  .break-all { word-break: break-all; }
  .leading-snug { line-height: 1.4; }
  .truncate { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

  /* Colors — all driven by --wg-* custom properties (set by the host) */
  .bg-wg-bg { background-color: var(--wg-bg); }
  .bg-wg-surface { background-color: var(--wg-surface); }
  .bg-wg-surface-hover { background-color: var(--wg-surface-hover); }
  .text-wg-text { color: var(--wg-text); }
  .text-wg-text-dim { color: var(--wg-text-dim); }
  .text-wg-primary { color: var(--wg-primary); }
  .text-wg-error { color: var(--wg-error); }
  .text-wg-success { color: var(--wg-success); }
  .text-wg-warning { color: var(--wg-warning); }

  .bg-wg-phase-pending { background-color: var(--wg-phase-pending); }
  .bg-wg-phase-running { background-color: var(--wg-phase-running); }
  .bg-wg-phase-idle { background-color: var(--wg-phase-idle); }
  .bg-wg-phase-faulted { background-color: var(--wg-phase-faulted); }
  .bg-wg-phase-cancelled { background-color: var(--wg-phase-cancelled); }

  .bg-wg-port-value { background-color: var(--wg-port-value); }
  .bg-wg-port-stream { background-color: var(--wg-port-stream); }
  .bg-wg-port-future { background-color: var(--wg-port-future); }
  .text-wg-port-value { color: var(--wg-port-value); }
  .text-wg-port-stream { color: var(--wg-port-stream); }
  .text-wg-port-future { color: var(--wg-port-future); }

  .text-wg-bg { color: var(--wg-bg); }

  /* Borders */
  .border { border-width: 1px; }
  .border-b { border-bottom-width: 1px; }
  .border-l-3 { border-left-width: 3px; }
  .border-transparent { border-color: transparent; }
  .border-wg-surface { border-color: var(--wg-surface); }
  .border-wg-bg { border-color: var(--wg-bg); }
  .border-wg-text-dim { border-color: var(--wg-text-dim); }
  .border-wg-primary { border-color: var(--wg-primary); }
  .border-wg-error { border-color: var(--wg-error); }
  .rounded { border-radius: 0.25rem; }
  .rounded-sm { border-radius: 0.125rem; }
  .rounded-md { border-radius: 0.375rem; }

  /* Cursor */
  .cursor-pointer { cursor: pointer; }
  .cursor-grab { cursor: grab; }
  .cursor-not-allowed { cursor: not-allowed; }

  /* Opacity */
  .opacity-40 { opacity: 0.4; }
  .opacity-85 { opacity: 0.85; }

  /* Transitions */
  .transition-opacity { transition-property: opacity; transition-duration: 0.15s; }

  /* Separator */
  .separator { width: 1px; height: 1.25rem; background: var(--wg-text-dim); margin: 0 0.25rem; }

  /* Scrollbar */
  ::-webkit-scrollbar { width: 6px; }
  ::-webkit-scrollbar-track { background: var(--wg-bg); }
  ::-webkit-scrollbar-thumb { background: var(--wg-surface-hover); border-radius: 3px; }
  ::-webkit-scrollbar-thumb:hover { background: var(--wg-overlay); }

  /* Shadows */
  .shadow-sm { box-shadow: 0 1px 3px rgba(0,0,0,0.3); }
  .shadow-md { box-shadow: 0 2px 8px rgba(0,0,0,0.35); }

  /* Semi-transparent backgrounds */
  .bg-wg-bg-50 { background-color: color-mix(in srgb, var(--wg-bg) 50%, transparent); }
  .bg-wg-surface-50 { background-color: color-mix(in srgb, var(--wg-surface) 50%, transparent); }
  .bg-wg-error-10 { background-color: color-mix(in srgb, var(--wg-error) 10%, transparent); }
  .bg-wg-warning-10 { background-color: color-mix(in srgb, var(--wg-warning) 10%, transparent); }
  .bg-wg-primary-10 { background-color: color-mix(in srgb, var(--wg-primary) 10%, transparent); }
  .bg-wg-primary-5 { background-color: color-mix(in srgb, var(--wg-primary) 5%, transparent); }

  /* Extra borders */
  .border-l-2 { border-left-width: 2px; }
  .border-wg-warning { border-color: var(--wg-warning); }
  .border-wg-success { border-color: var(--wg-success); }
  .border-wg-port-value { border-color: var(--wg-port-value); }
  .border-wg-port-stream { border-color: var(--wg-port-stream); }
  .border-wg-port-future { border-color: var(--wg-port-future); }
  .rounded-lg { border-radius: 0.5rem; }

  /* Extra spacing */
  .p-4 { padding: 1rem; }
  .px-1 { padding-left: 0.25rem; padding-right: 0.25rem; }
  .px-1\\.5 { padding-left: 0.375rem; padding-right: 0.375rem; }
  .py-0\\.5 { padding-top: 0.125rem; padding-bottom: 0.125rem; }
  .mb-0\\.5 { margin-bottom: 0.125rem; }
  .mb-4 { margin-bottom: 1rem; }
  .mt-3 { margin-top: 0.75rem; }
  .mr-1 { margin-right: 0.25rem; }
  .ml-2 { margin-left: 0.5rem; }
  .gap-0\\.5 { gap: 0.125rem; }
  .gap-4 { gap: 1rem; }

  /* Extra text */
  .text-2xs { font-size: 0.625rem; line-height: 0.875rem; }
  .text-lg { font-size: 1rem; line-height: 1.5rem; }
  .italic { font-style: italic; }
  .select-none { user-select: none; }

  /* Focus styles */
  .focus-ring:focus { border-color: var(--wg-primary); outline: none; box-shadow: 0 0 0 2px color-mix(in srgb, var(--wg-primary) 25%, transparent); }

  /* Interactive states */
  .hover\\:bg-wg-surface-hover:hover { background-color: var(--wg-surface-hover); }
  .hover\\:border-wg-primary:hover { border-color: var(--wg-primary); }
  .disabled\\:opacity-40:disabled { opacity: 0.4; }
  .disabled\\:cursor-not-allowed:disabled { cursor: not-allowed; }

  /* Transition */
  .transition-all { transition: all 0.15s ease; }
`;
