// JSPI (JavaScript Promise Integration) is how jco lets a component wait on JavaScript: an import
// that returns a promise suspends the component (`WebAssembly.Suspending`), and an export that
// may suspend returns a promise (`WebAssembly.promising`). jco's component-model async runtime
// needs it for every async component, and the kit's blocking WASI waits use it.

/** Whether this JavaScript engine has JSPI. */
export function hasJspi(): boolean {
  const wasm = (globalThis as { WebAssembly?: Record<string, unknown> }).WebAssembly;
  return typeof wasm?.Suspending === 'function' && typeof wasm?.promising === 'function';
}

/** The message `requireJspi` throws, for a page that wants to show it. */
export const JSPI_MISSING =
  'This needs WebAssembly JSPI (WebAssembly.Suspending and WebAssembly.promising), which this browser does not ' +
  'provide. Use a current Chromium-based browser (Chrome or Edge 137 or newer).';

/** Throws an `Error` with a clear message unless the engine has JSPI. */
export function requireJspi(): void {
  if (!hasJspi()) throw new Error(JSPI_MISSING);
}
