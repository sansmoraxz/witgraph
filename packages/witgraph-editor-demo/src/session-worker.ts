/**
 * Where the demo's graphs run: a thread of their own, so a node that spins cannot freeze
 * the page. Capabilities are provided here, for the demo the page names as `name`, and
 * only values reach the page.
 */

import { monotonicClock } from '@bytecodealliance/preview3-shim/clocks';
import { type Capability, type Endpoint, serveSession } from 'witgraph-loader';
import { type Cnn, cpu } from './accel/cnn.ts';
import { webgpu } from './accel/webgpu.ts';
import { webnn } from './accel/webnn.ts';
import { camera, framesFrom } from './data/camera.ts';
import { tensorView } from './data/tensor-view.ts';
import { type Backend, cpu as cpuTensors, tensors } from './data/tensors.ts';
import { webgpu as webgpuTensors } from './data/webgpu.ts';
import { webnn as webnnTensors } from './data/webnn.ts';

/**
 * What a demo runs on, when it is not the CPU: what provides `test:accel/cnn`, and where
 * tensors live.
 */
const backends: Record<
  string,
  { accelerator?: () => Promise<Cnn>; tensors?: () => Promise<Backend> }
> = {
  webnn: { accelerator: webnn },
  'tensors-webgpu': { tensors: webgpuTensors },
  'tensors-webnn': { tensors: webnnTensors },
  'camera-webgpu': { tensors: webgpuTensors },
  'camera-webnn': { tensors: webnnTensors },
};

/**
 * Implements `test:math/ops@0.1.0` in pure JS for the `calc` component.
 * Demonstrates sync, async, and streaming capability provision.
 */
function mathOps() {
  let callCount = 0;
  return {
    double: (x: number) => {
      callCount++;
      return x * 2;
    },
    slowDouble: async (x: number) => x * 2,
    count: async function* (n: number) {
      for (let i = 0; i < n; i++) yield i;
    },
    calls: () => callCount,
  };
}

/**
 * Implements `test:mqtt/source@0.1.0` in pure JS for the `mqtt-node`.
 * Returns canned sensor readings as byte buffers, then `undefined` to
 * signal end-of-feed.
 */
function mqttSource() {
  const feed = [
    new Uint8Array([0x48, 0x45, 0x4c, 0x4c, 0x4f]), // "HELLO"
    new Uint8Array([0x74, 0x65, 0x6d, 0x70, 0x3a, 0x32, 0x33]), // "temp:23"
    new Uint8Array([0x6f, 0x6b]), // "ok"
  ];
  let index = 0;
  return {
    nextMessage: () => (index < feed.length ? feed[index++] : undefined),
  };
}

const page = self as unknown as Endpoint & Parameters<typeof framesFrom>[0];
const frames = framesFrom(page);

async function setUp() {
  const demo = backends[self.name] ?? {};
  const cnn = demo.accelerator ? await demo.accelerator() : cpu();
  const backend = demo.tensors ? await demo.tensors() : cpuTensors();
  // One import object for every node, so a tensor one node makes is one another takes.
  const tensorImport = tensors(backend);
  const cameraImport = camera(backend, tensorImport.Tensor, frames);
  return {
    capabilities(capability: Capability): object | undefined {
      const iface = capability.implements ?? capability.interface;
      if (iface.startsWith('wasi:clocks/monotonic-clock')) return monotonicClock;
      if (iface.startsWith('wasi:webgpu/webgpu')) return webgpu;
      if (iface.startsWith('test:accel/cnn')) return cnn;
      if (iface.startsWith('test:data/tensor')) return tensorImport;
      if (iface.startsWith('test:camera/camera')) return cameraImport;
      // Asked once per instantiation: fresh state each time.
      if (iface.startsWith('test:math/ops')) return mathOps();
      if (iface.startsWith('test:mqtt/source')) return mqttSource();
      return undefined;
    },
    views: { tensor: tensorView },
  };
}

serveSession(page, setUp());
