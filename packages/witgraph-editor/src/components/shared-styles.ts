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

  /* Colors */
  .bg-wg-bg { background-color: #1e1e2e; }
  .bg-wg-surface { background-color: #313244; }
  .bg-wg-surface-hover { background-color: #45475a; }
  .text-wg-text { color: #cdd6f4; }
  .text-wg-text-dim { color: #a6adc8; }
  .text-wg-primary { color: #89b4fa; }
  .text-wg-error { color: #f38ba8; }
  .text-wg-success { color: #a6e3a1; }
  .text-wg-warning { color: #fab387; }

  .bg-wg-phase-pending { background-color: #fab387; }
  .bg-wg-phase-running { background-color: #89b4fa; }
  .bg-wg-phase-idle { background-color: #a6e3a1; }
  .bg-wg-phase-faulted { background-color: #f38ba8; }
  .bg-wg-phase-cancelled { background-color: #f9e2af; }

  .bg-wg-port-value { background-color: #89b4fa; }
  .bg-wg-port-stream { background-color: #cba6f7; }
  .bg-wg-port-future { background-color: #f9e2af; }
  .text-wg-port-value { color: #89b4fa; }
  .text-wg-port-stream { color: #cba6f7; }
  .text-wg-port-future { color: #f9e2af; }

  .text-wg-bg { color: #1e1e2e; }

  /* Borders */
  .border { border-width: 1px; }
  .border-b { border-bottom-width: 1px; }
  .border-l-3 { border-left-width: 3px; }
  .border-transparent { border-color: transparent; }
  .border-wg-surface { border-color: #313244; }
  .border-wg-bg { border-color: #1e1e2e; }
  .border-wg-text-dim { border-color: #a6adc8; }
  .border-wg-primary { border-color: #89b4fa; }
  .border-wg-error { border-color: #f38ba8; }
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
  .separator { width: 1px; height: 1.25rem; background: #a6adc8; margin: 0 0.25rem; }

  /* Scrollbar */
  ::-webkit-scrollbar { width: 6px; }
  ::-webkit-scrollbar-track { background: #1e1e2e; }
  ::-webkit-scrollbar-thumb { background: #45475a; border-radius: 3px; }
  ::-webkit-scrollbar-thumb:hover { background: #585b70; }

  /* Shadows */
  .shadow-sm { box-shadow: 0 1px 3px rgba(0,0,0,0.3); }
  .shadow-md { box-shadow: 0 2px 8px rgba(0,0,0,0.35); }

  /* Semi-transparent backgrounds */
  .bg-wg-bg-50 { background-color: rgba(30,30,46,0.5); }
  .bg-wg-surface-50 { background-color: rgba(49,50,68,0.5); }
  .bg-wg-error-10 { background-color: rgba(243,139,168,0.1); }
  .bg-wg-warning-10 { background-color: rgba(250,179,135,0.1); }
  .bg-wg-primary-10 { background-color: rgba(137,180,250,0.1); }
  .bg-wg-primary-5 { background-color: rgba(137,180,250,0.05); }

  /* Extra borders */
  .border-l-2 { border-left-width: 2px; }
  .border-wg-warning { border-color: #fab387; }
  .border-wg-success { border-color: #a6e3a1; }
  .border-wg-port-value { border-color: #89b4fa; }
  .border-wg-port-stream { border-color: #cba6f7; }
  .border-wg-port-future { border-color: #f9e2af; }
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
  .focus-ring:focus { border-color: #89b4fa; outline: none; box-shadow: 0 0 0 2px rgba(137,180,250,0.25); }

  /* Interactive states */
  .hover\\:bg-wg-surface-hover:hover { background-color: #45475a; }
  .hover\\:border-wg-primary:hover { border-color: #89b4fa; }
  .disabled\\:opacity-40:disabled { opacity: 0.4; }
  .disabled\\:cursor-not-allowed:disabled { cursor: not-allowed; }

  /* Transition */
  .transition-all { transition: all 0.15s ease; }
`;
