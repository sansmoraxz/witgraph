// The shim publishes no types: the little of it the demo names.
declare module '@wasi-gfx/wasi-gfx-shim/webgpu' {
  export class Gpu {
    constructor(gpu: Pick<GPU, 'requestAdapter'> | { requestAdapter(): Promise<null> });
  }
}
