// Stand-in frontend: exercises exactly what the real one needs from the shell.
const $ = (id) => document.getElementById(id);
const show = (id, text, ok) => { $(id).textContent = text; $(id).className = ok ? 'ok' : 'bad'; };
const result = { origin: location.origin, frames: 0, bytes: 0, kinds: { 1: 0, 2: 0, 3: 0, 4: 0 } };
$('origin').textContent = location.origin;

window.__result = result;

async function main() {
  // 1. Where is the server? Inside the Tauri shell a command says so; in a
  //    browser the page was served by it, so it is the page's own origin.
  let info;
  let base;
  if (window.__TAURI__) {
    try {
      info = await window.__TAURI__.core.invoke('server_info');
      result.command = info;
      base = info.http_url;
      show('command', `${info.name} ${info.version}, ${info.http_url}`, info.name === 'splat-live');
    } catch (e) {
      result.command_error = String(e);
      show('command', `ERREUR ${e}`, false);
      return;
    }
  } else {
    base = location.origin;
    info = await (await fetch(`${base}/api/info`)).json();
    $('command').textContent = 'hors de Tauri : origine de la page';
  }
  result.secure = window.isSecureContext;
  result.xr = 'xr' in navigator;

  // 2. The same JSON over HTTP (cross-origin from the webview: needs CORS and CSP connect-src).
  try {
    const viaHttp = await (await fetch(`${base}/api/info`)).json();
    result.info = viaHttp;
    result.fetch_matches = viaHttp.http_url === info.http_url && viaHttp.protocol === info.protocol;
    show('fetch', `protocole ${viaHttp.protocol}, ${viaHttp.clients} client(s)`, result.fetch_matches);
  } catch (e) {
    result.fetch_error = String(e);
    show('fetch', `ERREUR ${e}`, false);
  }

  // 3. The WebSocket stream.
  try {
    const ws = new WebSocket(base.replace(/^http/, 'ws') + info.ws_path);
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => { result.ws = 'open'; show('ws', 'ouvert', true); };
    ws.onerror = () => { result.ws = 'error'; show('ws', 'ERREUR', false); };
    ws.onclose = (e) => { result.ws_close = e.code; show('ws', `fermé (${e.code})`, false); };
    ws.onmessage = (e) => {
      const view = new Uint8Array(e.data);
      result.frames += 1;
      result.bytes += view.length;
      result.kinds[view[4]] = (result.kinds[view[4]] || 0) + 1;
      result.magic = String.fromCharCode(...view.slice(0, 4));
      $('frames').textContent = result.frames;
      $('bytes').textContent = result.bytes;
      $('kinds').textContent = `State ${result.kinds[1]}, Layer ${result.kinds[2]}, Preview ${result.kinds[3]}, Stats ${result.kinds[4]}`;
    };
  } catch (e) {
    result.ws_error = String(e);
    show('ws', `ERREUR ${e}`, false);
  }

  // 4. The LAN switch (a JSON POST: preflight from the webview origin) and the QR image.
  try {
    const response = await fetch(`${base}/api/lan`, {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ enabled: true }),
    });
    result.lan_status = response.status;
    if (!response.ok) throw new Error(`refusé (${response.status})`);
    const lan = (await response.json()).lan;
    result.lan = lan;
    show('lan', `${response.status}, ${lan.urls.length} URL, empreinte ${lan.fingerprint.slice(0, 17)}...`, lan.enabled);
    $('qr').onload = () => { result.qr = `${$('qr').naturalWidth}x${$('qr').naturalHeight}`; };
    $('qr').onerror = () => { result.qr = 'error'; };
    $('qr').src = `${base}/api/qr.svg`;
  } catch (e) {
    result.lan_error = String(e);
    show('lan', `ERREUR ${e}`, false);
  }

  // 5. WebAssembly is allowed by the CSP (the real frontend is wasm).
  try {
    // Streaming compilation insists on the application/wasm MIME type.
    const module = await WebAssembly.compileStreaming(fetch('probe.wasm'));
    result.wasm = module instanceof WebAssembly.Module;
    show('wasm', 'compilation en flux autorisée', true);
  } catch (e) {
    result.wasm_error = String(e);
    show('wasm', `ERREUR ${e}`, false);
  }

  // 6. Recording and export, inside the shell only (in a browser this
  //    page must not leave files in the user's data folder): the record
  //    button, a recording read by byte ranges across origins, the pairing
  //    code, and the two exports a web view cannot download by itself.
  if (window.__TAURI__) {
    try {
      const post = (body) => fetch(`${base}/api/record`, {
        method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body),
      }).then((response) => response.json());
      const started = await post({ action: 'start', name: 'Essai du shell' });
      await new Promise((resolve) => setTimeout(resolve, 1500));
      const stopped = await post({ action: 'stop' });
      const saved = stopped.saved;
      result.record = !!(started.recording && saved && saved.duration_s > 1 && saved.size > 1000);
      const ranged = await fetch(`${base}/${saved.url}`, { headers: { range: 'bytes=0-7' } });
      const head = new TextDecoder().decode(await ranged.arrayBuffer());
      result.range = ranged.status === 206 && head.startsWith('GSREC1') && /^bytes 0-7\/\d+$/.test(ranged.headers.get('content-range') || '');
      const listed = await (await fetch(`${base}/api/recordings`)).json();
      result.listed = listed.recordings.some((entry) => entry.name === saved.name);
      const copy = await window.__TAURI__.core.invoke('export_recording', { name: saved.name, kind: 'recording' });
      const archive = await window.__TAURI__.core.invoke('export_recording', { name: saved.name, kind: 'share' });
      result.exports = copy.endsWith('.gsrec') && archive.endsWith('.zip');
      // Names that are not recordings are refused with a sentence.
      result.export_refused = await window.__TAURI__.core.invoke('export_recording', { name: '../x', kind: 'share' }).then(() => false, (e) => typeof e === 'string');
      result.reveal_refused = await window.__TAURI__.core.invoke('reveal', { target: '/etc/passwd' }).then(() => false, (e) => typeof e === 'string');
      const pair = new Image();
      pair.onload = () => { result.pair = `${pair.naturalWidth}x${pair.naturalHeight}`; };
      pair.onerror = () => { result.pair = 'error'; };
      pair.src = `${base}/api/pair.svg`;
      show('record', `${saved.file}, ${saved.duration_s.toFixed(1)} s, exports : ${copy} et ${archive}`, result.record && result.range && result.exports);
    } catch (e) {
      result.record_error = String(e);
      show('record', `ERREUR ${e}`, false);
    }
  } else {
    $('record').textContent = 'hors de Tauri : non essayé';
  }

  // Report to the host log through a request the server logs in verbose mode.
  setInterval(() => {
    fetch(`${base}/api/info?report=${encodeURIComponent(JSON.stringify({
      origin: result.origin, command: !!result.command, fetch: result.fetch_matches, ws: result.ws, frames: result.frames,
      bytes: result.bytes, kinds: result.kinds, magic: result.magic, lan: result.lan && result.lan.enabled, qr: result.qr,
      wasm: result.wasm, record: result.record, range: result.range, listed: result.listed, exports: result.exports,
      export_refused: result.export_refused, reveal_refused: result.reveal_refused, pair: result.pair,
      errors: [result.command_error, result.fetch_error, result.ws_error, result.lan_error, result.wasm_error, result.record_error].filter(Boolean),
    }))}`).catch(() => {});
  }, 1000);
}
main();
