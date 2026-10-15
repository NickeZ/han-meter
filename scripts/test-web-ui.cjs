// Exercise the embedded page's polling and controls without a browser dependency.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const html = fs.readFileSync(`${__dirname}/../crates/han-config/src/settings.html`, 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const elements = new Map([...html.matchAll(/id="([^"]+)"/g)].map(([, id]) => [id, {
  textContent: '', value: '', disabled: false, scrollHeight: 100, scrollTop: 0, clientHeight: 100,
  listeners: {}, attributes: {},
  addEventListener(event, callback) { this.listeners[event] = callback; },
  setAttribute(name, value) { this.attributes[name] = value; },
}]));
const events = [{sequence: 1, uptime_ms: 1000, message: 'UART FifoOverflowed; count=1'}];
let boot = 7;
let fail = false;
let download;
let clicked = false;
const requests = [];
const context = vm.createContext({
  document: {
    getElementById: id => elements.get(id),
    createElement: () => ({click() { clicked = true; }}),
  },
  AbortSignal, Blob,
  URL: {createObjectURL(blob) { download = blob; return 'blob:test'; }, revokeObjectURL() {}},
  setTimeout() {},
  async fetch(url) {
    requests.push(url);
    if (fail) throw new Error('offline');
    if (url === '/api/mqtt') return {ok: true, json: async () => ({
      host: 'private-broker', username: 'private-user', port: 1883,
      status: 'connected', storage_available: true, password_set: true,
    })};
    const after = Number(new URL(url, 'http://device').searchParams.get('after'));
    const remaining = events.filter(event => event.sequence > after);
    const page = remaining.slice(0, 4);
    return {ok: true, json: async () => ({
      boot_id: boot, uptime_ms: 20_000, valid_frames: 15, invalid_frames: 2,
      uart_errors: 45, uart_error_types: {overflow: 40, framing: 5, glitch: 0, parity: 0, other: 0},
      dma: {received_bytes: 12345, errors: 2, overruns: 1},
      last_valid_age_ms: 200, last_interval_ms: 1000, max_interval_ms: 3000,
      log: {entries: page, next_after: page.at(-1)?.sequence ?? after, more: remaining.length > 4, lost: false},
    })};
  },
});
const run = code => vm.runInContext(code, context);
const flush = () => new Promise(resolve => setImmediate(resolve));
const click = id => elements.get(id).listeners.click();

(async () => {
  vm.runInContext(script, context);
  await flush();
  assert.match(elements.get('log').textContent, /FifoOverflowed/);
  assert.match(elements.get('diagnostic-summary').textContent, /Overflow: 40 · Framing: 5/);
  assert.match(elements.get('diagnostic-summary').textContent, /Longest: 3\.000 s/);
  assert.match(elements.get('diagnostic-summary').textContent, /DMA bytes: 12345 · Errors: 2 · Buffer overruns: 1/);

  // Polling from the previous cursor must not duplicate existing events.
  await run('refreshDiagnostics()');
  assert.equal(elements.get('log').textContent.split('\n').length, 1);
  click('pause-log');
  const beforePause = requests.length;
  events.push({sequence: 2, uptime_ms: 2000, message: 'Gap between valid telegrams: 3000 ms'});
  await run('refreshDiagnostics()');
  assert.equal(requests.length, beforePause);
  assert.equal(elements.get('pause-log').attributes['aria-pressed'], 'true');
  click('pause-log');
  await run('refreshDiagnostics()');
  assert.match(elements.get('log').textContent, /Gap between valid telegrams/);

  // Downloads include the received evidence but none of the settings fields.
  click('download-log');
  assert.ok(clicked);
  const text = await download.text();
  assert.match(text, /UART errors: 45/);
  assert.match(text, /DMA bytes: 12345 · Errors: 2 · Buffer overruns: 1/);
  assert.match(text, /FifoOverflowed/);
  assert.doesNotMatch(text, /private-broker|private-user|password/);

  // Offline polling preserves evidence, and a reboot starts from a fresh cursor.
  const previousLog = elements.get('log').textContent;
  fail = true;
  await run('refreshDiagnostics()');
  assert.equal(elements.get('log').textContent, previousLog);
  assert.match(elements.get('diagnostic-status').textContent, /unreachable/);
  fail = false;
  boot++;
  events.splice(0, events.length, {sequence: 1, uptime_ms: 100, message: 'New boot'});
  await run('refreshDiagnostics()');
  assert.match(elements.get('log').textContent, /Device restarted/);
  assert.match(elements.get('log').textContent, /New boot/);
  assert.doesNotMatch(elements.get('log').textContent, /FifoOverflowed/);

  // Catch up in bounded requests and keep browser memory bounded as well.
  for (let sequence = 2; sequence <= 300; sequence++) {
    events.push({sequence, uptime_ms: sequence * 1000, message: `event ${sequence}`});
  }
  let polls = 0;
  while (await run('refreshDiagnostics()')) assert.ok(++polls < 100);
  assert.equal(elements.get('log').textContent.split('\n').length, 256);
  assert.match(elements.get('log').textContent, /event 300$/);
  console.log('Web UI checks passed: counters, cursor, pause/resume, download, reconnect, reboot, bounded history.');
})().catch(error => { console.error(error); process.exitCode = 1; });
