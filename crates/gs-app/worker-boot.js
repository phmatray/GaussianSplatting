// Entry point of the pipeline Web Worker (a module worker). Loads the
// wasm-bindgen glue Trunk emits for crates/gs-worker; its `main` installs the
// message handler and posts {"type":"ready"}.
//
// Trunk does not content-hash worker files. The page passes its build id as
// `?v=`; forwarding it to both files keeps a worker cached from an older
// deployment from being paired with a newer page.
(async () => {
  const version = new URL(self.location.href).searchParams.get('v') || '';
  const suffix = version ? `?v=${encodeURIComponent(version)}` : '';
  const glue = await import(`./splat-worker.js${suffix}`);
  await glue.default({ module_or_path: new URL(`./splat-worker_bg.wasm${suffix}`, self.location.href) });
})().catch((error) => {
  self.postMessage(JSON.stringify({ type: 'error', message: `boot: ${error}` }));
});
