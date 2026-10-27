// Exercise the embedded page's polling and controls without a browser dependency.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

const html = fs.readFileSync(`${__dirname}/../crates/han-config/src/settings.html`, 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const makeElements = () => new Map([...html.matchAll(/id="([^"]+)"/g)].map(([, id]) => [id, {
  textContent: '', value: '', disabled: false, scrollHeight: 100, scrollTop: 0, clientHeight: 100,
  listeners: {}, attributes: {},
  addEventListener(event, callback) { this.listeners[event] = callback; },
  setAttribute(name, value) { this.attributes[name] = value; },
}]));
const elements = makeElements();
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

function bootloaderBrowser() {
  const elements = makeElements();
  const requests = [];
  const state = {confirm: false, mode: 'accepted', holdReads: null};
  const context = vm.createContext({
    document: {getElementById: id => elements.get(id)},
    AbortSignal, setTimeout() {},
    confirm(message) {
      assert.match(message, /MQTT.*stop/);
      return state.confirm;
    },
    async fetch(url, options = {}) {
      requests.push({url, options});
      if (url === '/api/bootloader') {
        if (state.mode === 'lost') throw new Error('connection closed');
        const accepted = state.mode === 'accepted';
        return {ok: accepted, status: accepted ? 202 : 503, json: async () => ({
          accepted, message: accepted ? 'Restarting into bootloader mode. Flash over USB.' : 'Bootloader entry disabled.',
        })};
      }
      if (state.holdReads) await state.holdReads;
      if (url === '/api/mqtt') return {ok: true, json: async () => ({
        host: 'broker', username: 'meter', port: 1883, status: 'connected', storage_available: false,
      })};
      return {ok: true, json: async () => ({
        boot_id: 1, uptime_ms: 100, valid_frames: 1, invalid_frames: 0, uart_errors: 0,
        uart_error_types: {overflow: 0, framing: 0, glitch: 0, parity: 0, other: 0},
        dma: {received_bytes: 100, errors: 0, overruns: 0},
        last_valid_age_ms: 0, last_interval_ms: null, max_interval_ms: null,
        log: {entries: [], next_after: 0, more: false, lost: false},
      })};
    },
  });
  vm.runInContext(script, context);
  return {elements, requests, state, run: code => vm.runInContext(code, context),
    click: () => elements.get('bootloader').listeners.click()};
}

async function checkBootloader() {
  const browser = bootloaderBrowser();
  await flush();
  assert.ok(browser.elements.get('fields').disabled);
  assert.ok(!browser.elements.get('bootloader').disabled);
  const initialRequests = browser.requests.length;
  await browser.click(); // Cancel confirmation.
  assert.equal(browser.requests.length, initialRequests);

  browser.state.confirm = true;
  browser.run('saving = true');
  await browser.click();
  assert.equal(browser.requests.length, initialRequests);
  browser.run('saving = false');
  browser.state.mode = 'rejected';
  await browser.click();
  assert.match(browser.elements.get('bootloader-message').textContent, /disabled/);
  assert.ok(!browser.elements.get('bootloader').disabled);
  assert.ok(browser.elements.get('fields').disabled, 'failed flash storage must stay disabled');

  // Replies already in flight must not overwrite bootloader status or resume polling.
  let finishReads;
  browser.state.holdReads = new Promise(resolve => { finishReads = resolve; });
  const reads = Promise.all([browser.run('refresh()'), browser.run('refreshDiagnostics()')]);
  browser.state.mode = 'accepted';
  const beforeBoot = browser.requests.filter(request => request.url === '/api/bootloader').length;
  await Promise.all([browser.click(), browser.click()]);
  finishReads();
  await reads;
  const commands = browser.requests.filter(request => request.url === '/api/bootloader');
  assert.equal(commands.length, beforeBoot + 1, 'double click must send only one command');
  assert.equal(commands.at(-1).options.method, 'POST');
  assert.equal(commands.at(-1).options.headers['X-Han-Settings'], '1');
  assert.equal(commands.at(-1).options.headers['Content-Type'], 'application/json');
  assert.deepEqual(JSON.parse(commands.at(-1).options.body), {confirm: true});
  assert.match(browser.elements.get('status').textContent, /Restarting into bootloader/);
  assert.match(browser.elements.get('diagnostic-status').textContent, /stopped for USB/);
  assert.ok(browser.elements.get('bootloader').disabled);
  const afterBoot = browser.requests.length;
  await browser.run('refresh()');
  await browser.run('refreshDiagnostics()');
  await browser.elements.get('settings').listeners.submit({preventDefault() {}});
  assert.equal(browser.requests.length, afterBoot);

  // A disconnect can mean the reset happened before its acknowledgement arrived.
  const lost = bootloaderBrowser();
  await flush();
  lost.state.confirm = true;
  lost.state.mode = 'lost';
  await lost.click();
  assert.match(lost.elements.get('status').textContent, /unconfirmed/);
  assert.match(lost.elements.get('bootloader-message').textContent, /may already/);
  const afterLost = lost.requests.length;
  await lost.click();
  await lost.run('refresh()');
  await lost.run('refreshDiagnostics()');
  assert.equal(lost.requests.length, afterLost, 'never automatically retry an uncertain command');
}

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
  await checkBootloader();
  console.log('Web UI checks passed: diagnostics, bounded history, bootloader confirmation, rejection, disconnects, duplicate clicks, and polling shutdown.');
})().catch(error => { console.error(error); process.exitCode = 1; });
