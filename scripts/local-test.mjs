import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import dgram from 'node:dgram';
import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import tls from 'node:tls';
import { readFile } from 'node:fs/promises';
import { setTimeout as delay } from 'node:timers/promises';
import { startLocalStack, binary } from './local-stack.mjs';

const stack = await startLocalStack({ build: !process.argv.includes('--no-build') });
let checks = 0;
const pass = (name) => { checks++; console.log('PASS ' + name); };

async function probe(name, overrides = {}, args = []) {
  const child = stack.launch('probe-' + name, binary('home-agent', 'probe', true), args, overrides);
  const timer = setTimeout(() => child.kill(), 65000);
  const code = await child.finished;
  clearTimeout(timer);
  const records = child.output.split(/\r?\n/).flatMap((line) => { try { return [JSON.parse(line)]; } catch { return []; } });
  if (code !== 0 || records.some((record) => !record.passed)) throw new Error(name + ':\n' + child.output);
  assert.ok(records.length > 0, name + ' produced no checks');
  checks += records.length;
  console.log('PASS ' + name + ' (' + records.length + ' checks)');
  await delay(200);
}

function signalingUrl(role, room, token = stack.env.SIGNALING_TOKEN) {
  const url = new URL(stack.env.SIGNALING_URL);
  url.search = new URLSearchParams({ role, room, token });
  return url;
}

async function rejectedUpgrade(role, room, token) {
  const url = signalingUrl(role, room, token);
  return new Promise((done, reject) => {
    const request = http.request({ hostname: url.hostname, port: url.port, path: url.pathname + url.search, headers: {
      Connection: 'Upgrade', Upgrade: 'websocket', 'Sec-WebSocket-Version': '13', 'Sec-WebSocket-Key': randomBytes(16).toString('base64'),
    } });
    request.once('response', (response) => { response.resume(); done(response.statusCode); });
    request.once('upgrade', (_response, socket) => { socket.destroy(); done(101); });
    request.once('error', reject);
    request.setTimeout(3000, () => request.destroy(new Error('WebSocket upgrade timed out')));
    request.end();
  });
}

async function socket(role, room) {
  const ws = new WebSocket(signalingUrl(role, room));
  ws.messages = [];
  ws.closed = new Promise((done) => ws.addEventListener('close', done, { once: true }));
  ws.addEventListener('message', (event) => ws.messages.push(JSON.parse(event.data)));
  await new Promise((done, reject) => {
    const timer = setTimeout(() => reject(new Error('WebSocket open timed out')), 3000);
    ws.addEventListener('open', () => { clearTimeout(timer); done(); }, { once: true });
    ws.addEventListener('error', (error) => { clearTimeout(timer); reject(error); }, { once: true });
  });
  return ws;
}

async function waitMessage(ws, predicate) {
  const deadline = Date.now() + 2000;
  while (Date.now() < deadline) { const found = ws.messages.find(predicate); if (found) return found; await delay(20); }
  throw new Error('Expected signaling message was not delivered');
}

async function authAndRooms() {
  assert.equal(await rejectedUpgrade('browser', 'auth-check', 'incorrect'), 401);
  pass('invalid signaling token rejected before upgrade');
  assert.equal(await rejectedUpgrade('invalid', 'auth-check'), 400);
  pass('invalid role rejected');
  const home = await socket('home', 'room-check');
  const first = await socket('browser', 'room-check');
  try {
    assert.equal(await rejectedUpgrade('browser', 'room-check'), 409);
    home.send(JSON.stringify({ type: 'answer', session: 'room-test', sdp: 'first-owner' }));
    await waitMessage(first, (message) => message.sdp === 'first-owner');
    pass('duplicate role does not overwrite the first connection');
    first.close();
    await first.closed;
    const second = await socket('browser', 'room-check');
    try {
      home.send(JSON.stringify({ type: 'answer', session: 'room-test', sdp: 'new-owner' }));
      await waitMessage(second, (message) => message.sdp === 'new-owner');
      pass('room remains usable after the old peer leaves');
    } finally { second.close(); await second.closed; }
  } finally { first.close(); home.close(); await home.closed; }
}

// A test-only packet adapter lets the Rust peer exercise browser-style TURN
// over TCP/TLS. The actual TURN authentication, allocation, relay and WebRTC
// payloads all pass through the server's TCP/TLS listener.
async function turnTunnel(secure) {
  const udp = dgram.createSocket('udp4');
  await new Promise((ready) => udp.bind(0, '127.0.0.1', ready));
  const stream = secure ? tls.connect({ host: '127.0.0.1', port: 18443, servername: 'localhost', ca: await readFile(stack.cert), ALPNProtocols: ['stun.turn'] }) : net.connect({ host: '127.0.0.1', port: 3478 });
  let failure;
  stream.on('error', (error) => { failure = error; });
  await new Promise((ready, reject) => {
    stream.once(secure ? 'secureConnect' : 'connect', ready);
    stream.once('error', reject);
  });
  let peer;
  let buffer = Buffer.alloc(0);
  udp.on('message', (packet, remote) => {
    peer = remote;
    const padding = packet[0] & 0xc0 ? (4 - packet.length % 4) % 4 : 0;
    stream.write(padding ? Buffer.concat([packet, Buffer.alloc(padding)]) : packet);
  });
  stream.on('data', (bytes) => {
    buffer = Buffer.concat([buffer, bytes]);
    while (buffer.length >= 4) {
      const channel = (buffer[0] & 0xc0) === 0x40;
      const size = buffer.readUInt16BE(2) + (channel ? 4 : 20);
      const padded = channel ? Math.ceil(size / 4) * 4 : size;
      if (buffer.length < padded) break;
      if (peer) udp.send(buffer.subarray(0, size), peer.port, peer.address);
      buffer = buffer.subarray(padded);
    }
    if (buffer.length > 128 * 1024) stream.destroy(new Error('TURN frame buffer overflow'));
  });
  return {
    url: 'turn:127.0.0.1:' + udp.address().port + '?transport=udp',
    close() { stream.destroy(); udp.close(); },
    assertHealthy() { if (failure) throw failure; },
  };
}

async function httpsGet(path, trustLocalCertificate = true) {
  const ca = trustLocalCertificate ? await readFile(stack.cert) : undefined;
  return new Promise((done, reject) => {
    https.get({ hostname: '127.0.0.1', port: 18443, servername: 'localhost', path, ca }, (response) => {
      let text = '';
      response.setEncoding('utf8');
      response.on('data', (chunk) => { text += chunk; });
      response.on('end', () => done({ status: response.statusCode, text }));
    }).on('error', reject);
  });
}

try {
  const tags = await fetch(stack.env.OLLAMA_BASE + '/api/tags').then((response) => response.json());
  assert.ok(tags.models.some((model) => model.name === 'qwen2.5:7b'));
  pass('Mock Ollama is available');
  const config = await fetch(stack.http + '/__local/config').then((response) => response.json());
  assert.equal(config.token, stack.env.SIGNALING_TOKEN);
  pass('local browser settings are filled automatically');
  await authAndRooms();
  await probe('direct');
  await probe('reconnect', {}, ['--connect-only']);
  await stack.restartSignaling();
  await probe('agent-auto-reconnect', {}, ['--connect-only']);
  await probe('turn-udp', { PROBE_TURN_URL: stack.env.TURN_URL });
  await probe('turn-invalid-credentials', { PROBE_TURN_URL: stack.env.TURN_URL, TURN_PASS: 'invalid-turn-password' }, ['--reject-turn']);
  for (const secure of [false, true]) {
    const tunnel = await turnTunnel(secure);
    try {
      await probe(secure ? 'turn-tls' : 'turn-tcp', { PROBE_TURN_URL: tunnel.url });
      tunnel.assertHealthy();
    } finally { tunnel.close(); }
  }
  const health = await httpsGet('/health');
  assert.equal(health.status, 200);
  assert.equal(health.text, 'ok');
  pass('HTTPS and TURN share the TLS listener');
  const page = await httpsGet('/');
  assert.equal(page.status, 200);
  assert.ok(page.text.includes('<div id="root">'));
  pass('production frontend is served over HTTPS');
  await assert.rejects(httpsGet('/health', false), (error) => ['DEPTH_ZERO_SELF_SIGNED_CERT', 'SELF_SIGNED_CERT_IN_CHAIN'].includes(error.code));
  pass('untrusted TLS certificate is rejected');
  await stack.restartAgent({ SIGNALING_URL: stack.wss });
  await probe('secure-signaling', { SIGNALING_URL: stack.wss });
  await stack.restartAgent({ REQUEST_TIMEOUT_SECS: '1' });
  await probe('upstream-timeout', {}, ['--timeout-check']);
  const requests = await fetch(stack.env.OLLAMA_BASE + '/__mock__/requests').then((response) => response.json());
  assert.ok(!requests.some((request) => request.path === '/api/delete'));
  pass('denied paths never reach Ollama');
  assert.ok(requests.some((request) => request.scenario === 'burst' && request.aborted));
  pass('cancellation closes the upstream HTTP request');
  assert.ok(['stall', 'stall-stream'].every((scenario) => requests.some((request) => request.scenario === scenario && request.aborted)));
  pass('timeouts close stalled upstream HTTP requests');
  await stack.restartAgent();
  console.log('All ' + checks + ' integration checks passed.');
  if (process.argv.includes('--keep')) {
    console.log('Local app ready: ' + stack.http + ' — click 连接, then send a message.');
    process.once('SIGINT', () => stack.stop().catch(console.error));
    process.once('SIGTERM', () => stack.stop().catch(console.error));
  } else await stack.stop();
} catch (error) {
  console.error(error);
  await stack.stop();
  process.exitCode = 1;
}
