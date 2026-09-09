import test from 'node:test';
import assert from 'node:assert/strict';
import { setTimeout as delay } from 'node:timers/promises';
import { OllamaRemoteClient } from './webrtcClient.js';
import { readNdjson } from './ndjson.js';
import { Ollama } from 'ollama/browser';

class Channel extends EventTarget {
  readyState = 'open';
  bufferedAmount = 0;
  sent = [];
  send(text) { const frame = JSON.parse(text); this.sent.push(frame); this.onSend?.(frame, text); }
  close() { if (this.readyState === 'closed') return; this.readyState = 'closed'; this.onclose?.(); this.dispatchEvent(new Event('close')); }
}

function transport(options = {}) {
  const client = new OllamaRemoteClient({ autoReconnect: false, ...options });
  const channel = new Channel();
  client.dc = channel;
  const frame = (value) => client._handleMessage(JSON.stringify(value));
  const respond = (id, body, status = 200, chunkSize = 8192) => {
    frame({ type: 'response', id, status, headers: { 'content-type': 'application/x-ndjson', 'x-test': 'forwarded' } });
    const bytes = Buffer.from(body);
    let seq = 0;
    for (let offset = 0; offset < bytes.length; offset += chunkSize) frame({ type: 'chunk', id, seq: seq++, data: bytes.subarray(offset, offset + chunkSize).toString('base64') });
    frame({ type: 'done', id });
  };
  return { client, channel, frame, respond };
}

test('fetch returns a Response and preserves UTF-8 split across byte frames', async () => {
  const { client, channel, respond } = transport();
  channel.onSend = (packet) => { if (packet.method) respond(packet.id, '你好🌍', 200, 2); };
  const response = await client.fetch(new Request('http://ollama.local/api/chat', { method: 'POST', body: '{}' }));
  assert.ok(response instanceof Response);
  assert.equal(response.headers.get('x-test'), 'forwarded');
  assert.equal(await response.text(), '你好🌍');
  assert.equal(client.pending.size, 0);
  assert.equal(channel.sent.filter((frame) => frame.type === 'ack').length, 5);
});

test('fetch keeps HTTP semantics and callback request reports HTTP errors', async () => {
  const { client, channel, respond } = transport();
  channel.onSend = (packet) => { if (packet.method) respond(packet.id, '{"error":"model unavailable"}', 503); };
  const response = await client.fetch('/api/chat', { method: 'POST' });
  assert.equal(response.status, 503);
  assert.equal((await response.json()).error, 'model unavailable');
  await assert.rejects(client.request('POST', '/api/chat').done, /model unavailable/);
});

test('large requests are fragmented below the DataChannel limit and reassemble exactly', async () => {
  const { client, channel, respond } = transport();
  const body = JSON.stringify({ messages: [{ content: 'x'.repeat(70000) + '你好🌍' }] });
  const fragments = [];
  channel.onSend = (packet, text) => {
    assert.ok(Buffer.byteLength(text) <= 16384);
    if (packet.type === 'request-fragment') {
      assert.equal(packet.seq, fragments.length);
      fragments.push(Buffer.from(packet.data, 'base64'));
      if (packet.done) {
        const request = JSON.parse(Buffer.concat(fragments));
        assert.equal(request.body, body);
        respond(request.id, '{"ok":true}');
      }
    }
  };
  assert.deepEqual(await (await client.fetch('/api/chat', { method: 'POST', body })).json(), { ok: true });
  assert.ok(fragments.length > 8);
});

test('response flow control acknowledges consumption instead of receipt', async () => {
  const { client, channel, frame } = transport();
  channel.onSend = (packet) => {
    if (!packet.method) return;
    frame({ type: 'response', id: packet.id, status: 200, headers: {} });
    for (let seq = 0; seq < 8; seq++) frame({ type: 'chunk', id: packet.id, seq, data: Buffer.alloc(8192, seq).toString('base64') });
  };
  const response = await client.fetch('/api/tags');
  assert.equal(channel.sent.filter((item) => item.type === 'ack').length, 0);
  const reader = response.body.getReader();
  assert.equal((await reader.read()).value.byteLength, 8192);
  assert.equal(channel.sent.filter((item) => item.type === 'ack').length, 1);
  await reader.cancel();
  assert.ok(channel.sent.some((item) => item.type === 'cancel'));
  assert.equal(client.pending.size, 0);
});

test('abort cancels upstream work and errors the response stream', async () => {
  const { client, channel, frame } = transport();
  channel.onSend = (packet) => { if (packet.method) frame({ type: 'response', id: packet.id, status: 200, headers: {} }); };
  const controller = new AbortController();
  const response = await client.fetch('/api/chat', { method: 'POST', signal: controller.signal });
  controller.abort();
  await assert.rejects(response.text(), { name: 'AbortError' });
  assert.equal(client.pending.size, 0);
  assert.ok(channel.sent.some((item) => item.type === 'cancel'));
});

test('upstream disconnect, timeout, and synchronous send errors settle requests', async () => {
  const { client, channel, frame } = transport({ requestTimeoutMs: 30 });
  channel.onSend = (packet) => {
    if (!packet.method) return;
    frame({ type: 'response', id: packet.id, status: 200, headers: {} });
    frame({ type: 'error', id: packet.id, status: 502, message: 'upstream disconnected' });
  };
  await assert.rejects((await client.fetch('/api/tags')).text(), /upstream disconnected/);
  channel.onSend = () => {};
  await assert.rejects(client.fetch('/api/tags'), /请求超时/);
  channel.onSend = () => { throw new Error('send failed'); };
  await assert.rejects(client.fetch('/api/tags'), /send failed/);
  assert.equal(client.pending.size, 0);
});

test('request body reads are bounded, cancellable, and included in the timeout', async () => {
  const { client, channel } = transport({ requestTimeoutMs: 1000 });
  let cancelled = false;
  const huge = new ReadableStream({
    pull(controller) { controller.enqueue(new Uint8Array(512 * 1024).fill(120)); },
    cancel() { cancelled = true; },
  });
  await assert.rejects(client.fetch('/api/chat', { method: 'POST', body: huge, duplex: 'half' }), /8 MiB/);
  assert.ok(cancelled);
  assert.equal(channel.sent.filter((item) => item.method || item.type === 'request-fragment').length, 0);

  const controller = new AbortController();
  const stalled = new ReadableStream({ cancel() { cancelled = true; } });
  cancelled = false;
  const pending = client.fetch('/api/chat', { method: 'POST', body: stalled, duplex: 'half', signal: controller.signal });
  controller.abort();
  await assert.rejects(pending, { name: 'AbortError' });
  assert.ok(cancelled);

  client.requestTimeoutMs = 30;
  const neverReady = new ReadableStream();
  await assert.rejects(client.fetch('/api/chat', { method: 'POST', body: neverReady, duplex: 'half' }), /请求超时/);
  assert.equal(client.pending.size, 0);
});

test('body uploads reserve request slots and stop when the peer disconnects', async () => {
  const { client } = transport();
  let cancelled = 0;
  const requests = Array.from({ length: 4 }, () => client.fetch('/api/chat', {
    method: 'POST', body: new ReadableStream({ cancel() { cancelled++; } }), duplex: 'half',
  }));
  const settled = Promise.all(requests.map((request) => assert.rejects(request, /连接已断开/)));
  await assert.rejects(client.fetch('/api/tags'), /4 个请求/);
  await client.disconnect();
  await settled;
  assert.equal(cancelled, 4);
  assert.equal(client.pending.size, 0);
});

test('manual reconnect waits for the previous WebSocket to release its room', async () => {
  const original = globalThis.WebSocket;
  const sockets = [];
  globalThis.WebSocket = class {
    static OPEN = 1;
    readyState = 0;
    constructor() {
      sockets.push(this);
      queueMicrotask(() => {
        if (this.readyState !== 0) return;
        this.readyState = 1;
        this.onopen?.();
        this.onmessage?.({ data: JSON.stringify({ type: 'ready', protocol: 1, peerOnline: false }) });
      });
    }
    send() {}
    close() {
      if (this.readyState >= 2) return;
      this.readyState = 2;
      setTimeout(() => { this.readyState = 3; this.onclose?.(); }, 20);
    }
  };
  const client = new OllamaRemoteClient({ signalingUrl: 'ws://localhost/ws', room: 'r', token: 'test', autoReconnect: false });
  try {
    const first = client.connect();
    const firstRejected = assert.rejects(first, /连接已断开/);
    await delay(1);
    assert.equal(client.connect(), first);
    const closing = client.disconnect();
    const next = client.connect();
    const nextRejected = assert.rejects(next, /连接已断开/);
    assert.equal(sockets.length, 1);
    await closing;
    assert.equal(sockets.length, 2);
    assert.equal(sockets[0].readyState, 3);
    await client.disconnect();
    await Promise.all([firstRejected, nextRejected]);
  } finally { await client.disconnect(); globalThis.WebSocket = original; }
});

test('invalid Token socket close rejects connect instead of hanging', async () => {
  const original = globalThis.WebSocket;
  globalThis.WebSocket = class {
    static OPEN = 1;
    readyState = 0;
    constructor() { queueMicrotask(() => { this.readyState = 3; this.onclose?.(); }); }
    close() { this.readyState = 3; this.onclose?.(); }
  };
  const client = new OllamaRemoteClient({ signalingUrl: 'ws://localhost/ws', room: 'r', token: 'invalid', autoReconnect: false });
  try { await assert.rejects(client.connect(), /信令连接已关闭/); } finally { await client.disconnect(); globalThis.WebSocket = original; }
});

test('buffers early ICE, emits the offer first, and rebuilds a fresh peer', async () => {
  const originals = [globalThis.WebSocket, globalThis.RTCPeerConnection];
  const peers = [];
  let socket;
  globalThis.RTCPeerConnection = class {
    remoteDescription = null;
    candidates = [];
    constructor() { peers.push(this); }
    createDataChannel() { this.channel = new Channel(); this.channel.readyState = 'connecting'; return this.channel; }
    async createOffer() { return { sdp: 'offer' }; }
    async setLocalDescription() { this.onicecandidate?.({ candidate: { toJSON: () => ({ candidate: 'local' }) } }); }
    async setRemoteDescription(value) {
      await delay(1);
      this.remoteDescription = value;
      this.channel.readyState = 'open';
      this.channel.onopen?.();
    }
    async addIceCandidate(value) { assert.ok(this.remoteDescription); this.candidates.push(value); }
    close() {}
  };
  globalThis.WebSocket = class {
    static OPEN = 1;
    readyState = 1;
    sent = [];
    constructor() { socket = this; queueMicrotask(() => { this.onopen?.(); this.deliver({ type: 'ready', protocol: 1, peerOnline: true }); }); }
    deliver(message) { this.onmessage?.({ data: JSON.stringify(message) }); }
    send(raw) {
      const frame = JSON.parse(raw);
      this.sent.push(frame);
      if (frame.type === 'offer') {
        this.deliver({ type: 'ice', session: frame.session, candidate: { candidate: 'remote' } });
        this.deliver({ type: 'answer', session: frame.session, sdp: 'answer' });
      }
    }
    close() { this.readyState = 3; this.onclose?.(); }
  };
  const client = new OllamaRemoteClient({ signalingUrl: 'ws://localhost/ws', room: 'r', token: 'test', autoReconnect: false });
  try {
    await client.connect();
    await delay(5);
    assert.equal(socket.sent.find((item) => item.type !== 'ping').type, 'offer');
    assert.equal(peers[0].candidates.length, 1);
    socket.deliver({ type: 'peer-left' });
    socket.deliver({ type: 'peer-joined' });
    await delay(10);
    assert.equal(peers.length, 2);
    assert.equal(client.state, 'connected');
  } finally { await client.disconnect(); [globalThis.WebSocket, globalThis.RTCPeerConnection] = originals; }
});

test('NDJSON handles split Unicode, the final line without newline, and malformed records', async () => {
  const text = '{"text":"你好🌍"}\n{"done":true}';
  const chunks = [...new TextEncoder().encode(text)].map((byte) => new Uint8Array([byte]));
  const stream = new ReadableStream({ start(controller) { for (const chunk of chunks) controller.enqueue(chunk); controller.close(); } });
  const records = [];
  for await (const value of readNdjson(stream)) records.push(value);
  assert.deepEqual(records, [{ text: '你好🌍' }, { done: true }]);
  const invalid = new ReadableStream({ start(controller) { controller.enqueue(new TextEncoder().encode('{broken}\n')); controller.close(); } });
  await assert.rejects(async () => { for await (const value of readNdjson(invalid)) void value; }, SyntaxError);
});

test('official Ollama SDK streams chat through the injected fetch', async () => {
  const { client, channel, respond } = transport();
  channel.onSend = (packet) => {
    if (packet.method) {
      assert.equal(packet.path, '/api/chat');
      assert.equal(JSON.parse(packet.body).model, 'mock:latest');
      respond(packet.id, '{"model":"mock:latest","message":{"role":"assistant","content":"你好"},"done":false}\n{"model":"mock:latest","message":{"role":"assistant","content":""},"done":true}\n');
    }
  };
  const ollama = new Ollama({ host: 'http://ollama.local', fetch: client.fetch });
  const response = await ollama.chat({ model: 'mock:latest', messages: [{ role: 'user', content: 'hello' }], stream: true });
  let text = '';
  for await (const part of response) text += part.message.content;
  assert.equal(text, '你好');
  assert.equal(client.pending.size, 0);
});
