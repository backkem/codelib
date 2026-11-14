// A Worker that runs one transpiled WASI 0.3 command (`wasi:cli/run@0.3.0#run`) under the kit,
// the way an embedder would: a world, the kit's WASI imports, the loader, the Worker runner.
// stdout and stderr are streamed to the page as they arrive and returned at the end.

import { instantiate, realWorld, serveRun, virtualWorld, wasiImports } from '/dist/index.js';

serveRun(async (init, ctx) => {
  // jco's runtime logs its steps when `process.env.JCO_DEBUG` is set; counting some of them
  // shows which of its code paths a run went through.
  const traced = {};
  if (init.trace) {
    globalThis.process = { env: { JCO_DEBUG: '1' } };
    console.debug = (first) => {
      for (const marker of init.trace) if (String(first).includes(marker)) traced[marker] = (traced[marker] ?? 0) + 1;
    };
  }
  const decoder = { stdout: new TextDecoder(), stderr: new TextDecoder() };
  const text = { stdout: '', stderr: '' };
  const sink = (stream) => (bytes) => {
    const chunk = decoder[stream].decode(bytes, { stream: true });
    text[stream] += chunk;
    ctx.post({ stream, chunk });
  };
  const worldOptions = { args: init.args, env: init.env, stdout: sink('stdout'), stderr: sink('stderr') };
  const world = init.seed !== undefined ? virtualWorld({ seed: BigInt(init.seed), ...worldOptions }) : realWorld(worldOptions);

  const t0 = performance.now();
  // `wrapCore` passes an `instantiateCore` that counts its calls and delegates to the default.
  let coreCalls = 0;
  const options = init.wrapCore
    ? {
        instantiateCore: (module, importObject) => {
          coreCalls++;
          return WebAssembly.instantiate(module, importObject);
        },
      }
    : {};
  const instance = await instantiate(init.module, wasiImports(world), options);
  const t1 = performance.now();
  ctx.running();
  let failed = null;
  try {
    await instance.call('wasi:cli/run@0.3.0#run');
  } catch (error) {
    failed = `${error?.name}: ${error?.message}`;
  }
  const t2 = performance.now();
  return { ...text, failed, traced, coreCalls, exportNames: instance.exportNames(), instantiateMs: t1 - t0, runMs: t2 - t1 };
});
