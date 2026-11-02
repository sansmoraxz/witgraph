import * as shim from '@wasi-gfx/wasi-gfx-shim/webgpu';

/**
 * `wasi:webgpu/webgpu` for jco, from wasi-gfx's shim over the browser's WebGPU.
 * The shim needs `navigator.gpu`; a browser without it offers no adapter here,
 * which is how the interface says there is no GPU.
 */
export const webgpu = {
  ...shim,
  getGpu: () => new shim.Gpu(navigator.gpu ?? { requestAdapter: async () => null }),
};
