import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import vm from 'node:vm';

const html = await readFile(new URL('../src/wifi/page.html', import.meta.url), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const snapshot = (boot = 'abc', version = 7, frame = 42) => ({
  boot_id: boot, version, frame_sequence: frame, bytes: 1234,
});
const bootId = '0123456789abcdef';
const triggerHeader = (event, frame = 42, boot = bootId) => `${boot}:${event}:${frame}`;
const response = (status, headers = {}, blob) => ({
  ok: status >= 200 && status < 300, status,
  headers: { get: name => headers[name] ?? null },
  blob,
});
const deferred = () => {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const flush = async () => {
  for (let i = 0; i < 30; i++) await Promise.resolve();
};

function harness() {
  let now = 0;
  let nextTimer = 0;
  const timers = new Map();
  const triggerWrites = [];
  const elements = Object.fromEntries(
    ['image', 'placeholder', 'state', 'trigger', 'frame', 'bytes'].map(id => {
      let text = id === 'trigger' ? html.match(/<p id="trigger"[^>]*>([^<]*)<\/p>/)[1] : '';
      return [id, {
        hidden: id === 'image',
        get textContent() { return text; },
        set textContent(value) {
          text = value;
          if (id === 'trigger') triggerWrites.push(value);
        },
      }];
    }),
  );
  const documentEvents = {};
  const windowEvents = {};
  const requests = [];
  const responses = [];
  const decodes = [];
  const created = [];
  const revoked = [];
  const document = {
    visibilityState: 'visible',
    getElementById: id => elements[id],
    addEventListener: (name, callback) => { documentEvents[name] = callback; },
  };
  const context = vm.createContext({
    document,
    window: { addEventListener: (name, callback) => { windowEvents[name] = callback; } },
    AbortController,
    setTimeout(callback, delay) {
      const id = ++nextTimer;
      timers.set(id, { callback, at: now + delay });
      return id;
    },
    clearTimeout: id => timers.delete(id),
    fetch: async (path, options) => {
      requests.push({ path, options });
      assert.equal(options.cache, 'no-store');
      assert.ok(options.signal instanceof AbortSignal);
      assert.ok(path.startsWith('/latest.jpg'));
      assert.ok(responses.length, 'unexpected fetch: ' + path);
      const response = responses.shift();
      return typeof response === 'function' ? response(options.signal) : response;
    },
    URL: {
      createObjectURL: blob => {
        const url = 'blob:test-' + (created.length + 1);
        created.push({ url, blob });
        return url;
      },
      revokeObjectURL: url => revoked.push(url),
    },
    Image: class {
      decode() { return decodes.length ? decodes.shift() : Promise.resolve(); }
    },
  });
  return {
    elements, requests, responses, decodes, created, revoked, document, triggerWrites,
    start: async () => { vm.runInContext(script, context); await flush(); },
    online: async () => { windowEvents.online(); await flush(); },
    visible: async () => { documentEvents.visibilitychange(); await flush(); },
    delay: () => Math.min(...Array.from(timers.values(), timer => timer.at - now)),
    async advance(ms) {
      const target = now + ms;
      while (true) {
        const next = Array.from(timers).sort((a, b) => a[1].at - b[1].at)[0];
        if (!next || next[1].at > target) break;
        now = next[1].at;
        timers.delete(next[0]);
        next[1].callback();
        await flush();
      }
      now = target;
      await flush();
    },
    jpeg(value = snapshot(), body = async () => ({ type: 'image/jpeg', size: value.bytes }), trigger) {
      const headers = {
        'X-Fridge-Boot': value.boot_id,
        'X-Fridge-Version': String(value.version),
        'X-Fridge-Frame': String(value.frame_sequence),
        'X-Fridge-Bytes': String(value.bytes),
        'X-Fridge-Trigger': trigger,
      };
      responses.push(response(200, headers, body));
    },
    unchanged(trigger) { responses.push(response(204, { 'X-Fridge-Trigger': trigger })); },
    http(status, trigger) { responses.push(response(status, { 'X-Fridge-Trigger': trigger })); },
  };
}

test('one request retrieves image and identity; unchanged polls are 200ms apart', async () => {
  const h = harness();
  h.jpeg();
  await h.start();
  assert.equal(h.requests.length, 1);
  assert.equal(h.requests[0].path, '/latest.jpg');
  assert.equal(h.elements.frame.textContent, '42');
  assert.equal(h.elements.image.hidden, false);
  assert.equal(h.delay(), 200);
  h.unchanged();
  await h.advance(200);
  assert.equal(h.requests[1].path, '/latest.jpg?boot=abc&version=7');
  assert.equal(h.created.length, 1);
  h.jpeg(snapshot('abc', 8, 43));
  await h.advance(200);
  assert.equal(h.elements.frame.textContent, '43');
  assert.deepEqual(h.revoked, ['blob:test-1']);
});

test('new boot identity is accepted even when counters reset', async () => {
  const h = harness();
  h.jpeg(); await h.start();
  h.jpeg(snapshot('def', 1, 0)); await h.advance(200);
  assert.equal(h.elements.frame.textContent, '0');
  h.unchanged(); await h.advance(200);
  assert.equal(h.requests.at(-1).path, '/latest.jpg?boot=def&version=1');
});

test('identity is accepted only after decoding, not after headers or body', async () => {
  const h = harness();
  h.jpeg(); await h.start();
  const decoding = deferred();
  h.decodes.push(decoding.promise);
  h.jpeg(snapshot('abc', 8, 43)); await h.advance(200);
  assert.equal(h.elements.image.src, 'blob:test-1');
  assert.equal(h.elements.frame.textContent, '42');
  decoding.reject(new Error('invalid JPEG')); await flush();
  assert.match(h.elements.state.textContent, /JPEG decode: invalid JPEG/);
  assert.doesNotMatch(h.elements.state.textContent, /オフライン/);
  assert.deepEqual(h.revoked, ['blob:test-2']);
  h.jpeg(snapshot('abc', 8, 43)); await h.advance(1000);
  assert.equal(h.requests.at(-1).path, '/latest.jpg?boot=abc&version=7');
  assert.equal(h.elements.frame.textContent, '43');
});

test('503 and network failures have bounded backoff, preserve the image and recover', async () => {
  const h = harness();
  h.jpeg(); await h.start();
  h.http(503); await h.advance(200);
  assert.match(h.elements.state.textContent, /画像がありません/);
  assert.equal(h.delay(), 1000);
  for (const delay of [2000, 4000, 5000, 5000]) {
    h.responses.push(() => { throw new Error('Failed to fetch'); });
    await h.advance(h.delay());
    assert.equal(h.delay(), delay);
    assert.match(h.elements.state.textContent, /\/latest.jpg: Failed to fetch/);
    assert.equal(h.elements.image.src, 'blob:test-1');
  }
  h.jpeg(snapshot('abc', 8)); await h.advance(5000);
  assert.equal(h.delay(), 200);
  assert.match(h.elements.state.textContent, /接続中/);
});

test('409 compatibility responses retry promptly without offline state', async () => {
  const h = harness();
  h.jpeg(); await h.start();
  h.http(409); await h.advance(200);
  assert.equal(h.delay(), 200);
  assert.doesNotMatch(h.elements.state.textContent, /オフライン/);
  h.jpeg(snapshot('abc', 8)); await h.advance(200);
  assert.equal(h.elements.image.src, 'blob:test-2');
});

for (const stage of ['fetch', 'blob', 'decode']) {
  test('10-second timeout covers ' + stage + ' and late results cannot overwrite a new image', async () => {
    const h = harness();
    const pending = deferred();
    if (stage === 'fetch') h.responses.push(() => pending.promise);
    else if (stage === 'blob') h.jpeg(snapshot(), () => pending.promise);
    else { h.decodes.push(pending.promise); h.jpeg(); }
    await h.start();
    await h.advance(9999);
    assert.equal(h.requests[0].options.signal.aborted, false);
    await h.advance(1);
    assert.equal(h.requests[0].options.signal.aborted, true);
    assert.match(h.elements.state.textContent, /timeout/);
    assert.equal(h.delay(), 1000);
    h.jpeg(snapshot('def', 1, 0)); await h.advance(1000);
    if (stage === 'fetch') pending.resolve(response(204));
    else if (stage === 'blob') pending.resolve({ size: 1234, type: 'image/jpeg' });
    else pending.resolve();
    await flush();
    assert.equal(h.elements.frame.textContent, '0');
    assert.equal(h.created.length, stage === 'decode' ? 2 : 1);
    assert.equal(h.elements.image.src, stage === 'decode' ? 'blob:test-2' : 'blob:test-1');
  });
}

test('wake events coalesce and do not overlap active downloads', async () => {
  const h = harness();
  const pending = deferred();
  h.responses.push(() => pending.promise); await h.start();
  await h.online(); await h.visible(); await h.online();
  assert.equal(h.requests.length, 1);
  h.jpeg(); pending.resolve(response(503)); await flush();
  assert.equal(h.delay(), 0);
  await h.advance(0);
  assert.equal(h.requests.length, 2);
  h.unchanged(); await h.online();
  assert.equal(h.requests.length, 3);
  h.document.visibilityState = 'hidden'; await h.visible();
  assert.equal(h.requests.length, 3);
});

test('malformed or missing identity headers are rejected before decoding', async () => {
  for (const value of [
    { ...snapshot(), boot_id: '../bad' },
    { ...snapshot(), boot_id: null },
    { ...snapshot(), version: '1e2' },
    { ...snapshot(), bytes: -1 },
    { ...snapshot(), frame_sequence: Number.MAX_SAFE_INTEGER + 1 },
  ]) {
    const h = harness(); h.jpeg(value); await h.start();
    assert.equal(h.elements.image.hidden, true);
    assert.equal(h.created.length, 0);
    assert.match(h.elements.state.textContent, /invalid status/);
    h.jpeg(); await h.advance(1000);
    assert.equal(h.elements.image.hidden, false);
  }
});

test('incomplete JPEG bodies retain the accepted identity and retry', async () => {
  const h = harness(); h.jpeg(); await h.start();
  h.jpeg(snapshot('abc', 8), async () => ({ size: 100 })); await h.advance(200);
  assert.match(h.elements.state.textContent, /incomplete JPEG/);
  assert.equal(h.elements.image.src, 'blob:test-1');
  h.jpeg(snapshot('abc', 8)); await h.advance(1000);
  assert.equal(h.requests.at(-1).path, '/latest.jpg?boot=abc&version=7');
});

test('trigger status is visible and accessible before any response', async () => {
  assert.match(html, /<p id="trigger" role="status" aria-live="polite">撮影トリガーを待っています<\/p>/);
  const h = harness();
  const pending = deferred();
  h.responses.push(() => pending.promise); await h.start();
  assert.equal(h.elements.trigger.textContent, '撮影トリガーを待っています');
  assert.equal(h.elements.trigger.hidden, false);
  pending.resolve(response(503)); await flush();
  assert.equal(h.elements.trigger.textContent, '撮影トリガーを待っています');
});

test('204 trigger changes are independent of the accepted JPEG version', async () => {
  const h = harness();
  h.jpeg(snapshot(), undefined, triggerHeader(1)); await h.start();
  h.unchanged(triggerHeader(3, 45)); await h.advance(200);
  assert.equal(h.elements.trigger.textContent, '撮影トリガーを検出しました（フレーム 45）。');
  assert.equal(h.elements.frame.textContent, '42');
  assert.equal(h.created.length, 1);
  assert.equal(h.requests.at(-1).path, '/latest.jpg?boot=abc&version=7');
  assert.equal(h.delay(), 200);
});

test('initial 503 reports a trigger without claiming a successful capture and survives network errors', async () => {
  const h = harness();
  h.http(503, triggerHeader(1, 0)); await h.start();
  const notification = '撮影トリガーを検出しました（フレーム 0）。';
  assert.equal(h.elements.trigger.textContent, notification);
  assert.match(h.elements.state.textContent, /画像がありません/);
  assert.equal(h.elements.image.hidden, true);
  h.responses.push(() => { throw new Error('Failed to fetch'); });
  await h.advance(1000);
  assert.match(h.elements.state.textContent, /オフライン/);
  assert.equal(h.elements.trigger.textContent, notification);
  h.jpeg(); await h.advance(2000);
  assert.equal(h.elements.trigger.textContent, notification);
});

test('trigger is displayed before JPEG body download and decode without overlapping polls', async () => {
  const h = harness();
  const body = deferred();
  const decoding = deferred();
  h.decodes.push(decoding.promise);
  h.jpeg(snapshot(), () => {
    assert.equal(h.elements.trigger.textContent, '撮影トリガーを検出しました（フレーム 43）。');
    return body.promise;
  }, triggerHeader(2, 43));
  await h.start();
  assert.equal(h.elements.trigger.textContent, '撮影トリガーを検出しました（フレーム 43）。');
  assert.equal(h.created.length, 0);
  assert.equal(h.elements.image.hidden, true);
  await h.online(); await h.visible(); await h.advance(200);
  assert.equal(h.requests.length, 1);
  body.resolve({ type: 'image/jpeg', size: 1234 }); await flush();
  assert.equal(h.elements.image.hidden, true);
  assert.equal(h.triggerWrites.length, 1);
  decoding.reject(new Error('invalid JPEG')); await flush();
  assert.match(h.elements.state.textContent, /JPEG decode/);
  assert.equal(h.elements.trigger.textContent, '撮影トリガーを検出しました（フレーム 43）。');
  assert.equal(h.requests.length, 1);
});

test('same boot and event are deduplicated even if frame or decimal formatting changes', async () => {
  const h = harness();
  h.jpeg(snapshot(), undefined, triggerHeader(1)); await h.start();
  h.unchanged(triggerHeader('01', 99, bootId.toUpperCase())); await h.advance(200);
  h.http(503, triggerHeader(1, 100)); await h.advance(200);
  assert.deepEqual(h.triggerWrites, ['撮影トリガーを検出しました（フレーム 42）。']);
  h.unchanged(triggerHeader(2, 42)); await h.advance(1000);
  assert.equal(h.triggerWrites.length, 2);
});

test('reboots accept reset event sequences and clear notifications when no trigger has occurred', async () => {
  const h = harness();
  h.jpeg(snapshot(), undefined, triggerHeader(10)); await h.start();
  const secondBoot = 'fedcba9876543210';
  h.unchanged(triggerHeader(1, 1, secondBoot)); await h.advance(200);
  assert.equal(h.elements.trigger.textContent, '撮影トリガーを検出しました（フレーム 1）。');
  h.unchanged(triggerHeader(0, 0, '0000000000000000')); await h.advance(200);
  assert.equal(h.elements.trigger.textContent, '撮影トリガーを待っています');
  h.unchanged(triggerHeader(0, 123, '0000000000000000')); await h.advance(200);
  assert.equal(h.triggerWrites.length, 3);
  h.unchanged(triggerHeader(1, 2, '0000000000000000')); await h.advance(200);
  assert.equal(h.elements.trigger.textContent, '撮影トリガーを検出しました（フレーム 2）。');
});

test('missing and malformed trigger headers are ignored without changing notification or dedup identity', async () => {
  const h = harness();
  h.jpeg(snapshot(), undefined, triggerHeader(1)); await h.start();
  for (const header of [
    undefined, '', 'abc:2:42', '0123456789abcdef0:2:42', '0123456789abcdeg:2:42',
    triggerHeader(-1), triggerHeader('1e2'), triggerHeader('1.5'), triggerHeader('+2'),
    triggerHeader(''), triggerHeader(4294967296), triggerHeader(2, 4294967296),
    triggerHeader(2, -1), triggerHeader(2, '1e2'), triggerHeader(2, ''),
    triggerHeader(2) + ':3', triggerHeader(2) + '\n', ' ' + triggerHeader(2),
  ]) {
    h.unchanged(header); await h.advance(200);
    assert.equal(h.elements.trigger.textContent, '撮影トリガーを検出しました（フレーム 42）。');
    assert.equal(h.triggerWrites.length, 1);
    assert.equal(h.delay(), 200);
  }
  h.unchanged(triggerHeader(2)); await h.advance(200);
  assert.equal(h.triggerWrites.length, 2);
  h.unchanged(triggerHeader(4294967295, 4294967295)); await h.advance(200);
  assert.equal(h.elements.trigger.textContent, '撮影トリガーを検出しました（フレーム 4294967295）。');
});

test('a trigger from an aborted fetch is ignored before processing headers', async () => {
  const h = harness();
  const pending = deferred();
  h.responses.push(() => pending.promise); await h.start();
  await h.advance(10000);
  h.http(503, triggerHeader(1)); await h.advance(1000);
  pending.resolve(response(503, { 'X-Fridge-Trigger': triggerHeader(2) })); await flush();
  assert.deepEqual(h.triggerWrites, ['撮影トリガーを検出しました（フレーム 42）。']);
});

test('204 without an accepted image is an error, not a blank successful display', async () => {
  const h = harness(); h.unchanged(); await h.start();
  assert.match(h.elements.state.textContent, /unexpected unchanged image/);
  assert.equal(h.elements.image.hidden, true);
  h.jpeg(); await h.advance(1000);
  assert.equal(h.elements.image.hidden, false);
});
