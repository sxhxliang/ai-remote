import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import http from 'node:http';
import net from 'node:net';
import { appendFile, mkdir, readFile, writeFile, access } from 'node:fs/promises';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

export const root = fileURLToPath(new URL('../', import.meta.url));
const localDir = resolve(root, '.local');
const suffix = process.platform === 'win32' ? '.exe' : '';
export const binary = (crate, name = crate, example = false) => resolve(process.env.LOCAL_BINARY_DIR || resolve(root, crate, 'target/debug'), example ? 'examples' : '', name + suffix);

export async function run(command, args, options = {}) {
  return new Promise((resolveRun, reject) => {
    const child = spawn(command, args, { cwd: root, windowsHide: true, stdio: 'inherit', ...options });
    child.once('error', reject);
    child.once('exit', (code) => code === 0 ? resolveRun() : reject(new Error(command + ' exited with code ' + code)));
  });
}

async function freePort(port) {
  const server = net.createServer();
  await new Promise((ready, reject) => {
    server.once('error', () => reject(new Error('Port ' + port + ' is occupied. Stop the previous local stack with node scripts/stop-local.mjs.')));
    server.listen(port, '127.0.0.1', ready);
  });
  await new Promise((done) => server.close(done));
}

async function waitFor(check, label, timeoutMs = 12000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await check()) return;
    await delay(100);
  }
  throw new Error('Timed out waiting for ' + label);
}

async function waitForHttp(url, label, child, validate) {
  await waitFor(async () => {
    if (child.startError || child.exitCode !== null || child.signalCode !== null) {
      throw new Error(label + ' exited before it was ready: ' + (child.startError?.message || child.exitCode || child.signalCode));
    }
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(1500) });
      if (!response.ok) { await response.body?.cancel(); return false; }
      return await validate(response);
    } catch { return false; }
  }, label, 30000);
}

export async function startLocalStack({ build = true, mockPort = 11434 } = {}) {
  if (!Number.isInteger(mockPort) || mockPort < 1 || mockPort > 65535 || [8080, 3478, 18443, 5173].includes(mockPort)) {
    throw new Error('Invalid Mock Ollama port');
  }
  await mkdir(localDir, { recursive: true });
  for (const port of [8080, mockPort, 3478, 18443, 5173]) await freePort(port);
  const vite = resolve(root, 'frontend/node_modules/vite/bin/vite.js');
  await access(vite).catch(() => { throw new Error('Install frontend dependencies first: npm --prefix frontend ci'); });
  if (build) {
    console.log('Building frontend');
    await run(process.execPath, [vite, 'build', resolve(root, 'frontend')]);
    for (const crate of ['home-agent', 'signaling-server', 'turn-server']) {
      console.log('Building ' + crate);
      await run('cargo', ['build', '--locked', '--manifest-path', crate + '/Cargo.toml', '--all-targets']);
    }
  }
  await access(resolve(root, 'frontend/dist/index.html')).catch(() => { throw new Error('Build frontend first: npm --prefix frontend run build'); });
  const certDir = resolve(localDir, 'certs');
  await mkdir(certDir, { recursive: true });
  const cert = resolve(certDir, 'cert.pem');
  const key = resolve(certDir, 'key.pem');
  try { await access(cert); await access(key); }
  catch { await run(binary('turn-server', 'dev-cert', true), [certDir]); }
  const token = randomBytes(24).toString('hex');
  const turnPassword = randomBytes(24).toString('hex');
  const env = {
    ...process.env,
    SIGNALING_TOKEN: token, ROOM_ID: 'local-mock', SIGNALING_URL: 'ws://127.0.0.1:8080/ws',
    SIGNALING_BIND: '127.0.0.1:8080', OLLAMA_BASE: 'http://127.0.0.1:' + mockPort, MOCK_OLLAMA_PORT: String(mockPort), REQUEST_TIMEOUT_SECS: '600',
    ALLOWED_PATHS: '/api/generate,/api/chat,/api/tags', ICE_SERVERS_JSON: '[]',
    STUN_URL: 'stun:127.0.0.1:3478', TURN_URL: 'turn:127.0.0.1:3478?transport=udp',
    TURN_USER: 'local-test', TURN_PASS: turnPassword, PUBLIC_IP: '127.0.0.1',
    TURN_UDP_BIND: '127.0.0.1:3478', TURN_TCP_BIND: '127.0.0.1:3478', TURN_TLS_BIND: '127.0.0.1:18443',
    RELAY_BIND: '127.0.0.1', RELAY_MIN_PORT: '49160', RELAY_MAX_PORT: '49200',
    TURN_IDLE_TIMEOUT_SECS: '600',
    TLS_CERT: cert, TLS_KEY: key, SSL_CERT_FILE: cert, HTTPS_UPSTREAM: '127.0.0.1:8080',
    FRONTEND_DIR: resolve(root, 'frontend/dist'),
  };
  delete env.ROOM_TOKENS_JSON;
  delete env.FORCE_RELAY;
  delete env.PROBE_TURN_URL;
  const children = new Set();
  const stack = { env, cert, key, children, http: 'http://127.0.0.1:5173', wss: 'wss://localhost:18443/ws' };
  let stopped = false;
  stack.launch = (name, executable, args = [], overrides = {}) => {
    const child = spawn(executable, args, { cwd: root, env: { ...env, ...overrides }, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    child.output = '';
    children.add(child);
    child.finished = new Promise((done) => {
      child.once('error', (error) => { child.startError = error; child.output += error.message; children.delete(child); done(-1); });
      child.once('exit', (code) => { children.delete(child); done(code); });
    });
    for (const stream of [child.stdout, child.stderr]) {
      stream.setEncoding('utf8');
      stream.on('data', (text) => { child.output = (child.output + text).slice(-300000); appendFile(resolve(localDir, name + '.log'), text).catch(() => {}); });
    }
    return child;
  };
  stack.kill = async (child) => {
    if (child && child.exitCode === null && child.signalCode === null) child.kill();
    if (child) await child.finished;
  };
  stack.stop = async () => {
    if (stopped) return;
    stopped = true;
    stack.control?.close();
    stack.control?.closeAllConnections();
    await Promise.all([...children].map((child) => stack.kill(child)));
    await writeFile(resolve(localDir, 'runtime.json'), JSON.stringify({ stopped: true }));
  };
  stack.restartAgent = async (overrides = {}) => {
    await stack.kill(stack.agent);
    await delay(200);
    stack.agent = stack.launch('home-agent', binary('home-agent'), [], overrides);
    await waitFor(() => stack.agent.output.includes('connected to signaling server as home'), 'home Agent');
  };
  stack.restartSignaling = async () => {
    await stack.kill(stack.signaling);
    stack.signaling = stack.launch('signaling', binary('signaling-server'));
    await waitForHttp('http://127.0.0.1:8080/health', 'signaling server', stack.signaling, async (response) => await response.text() === 'ok');
  };
  try {
    await stack.restartSignaling();
    stack.mock = stack.launch('mock-ollama', process.execPath, [resolve(root, 'scripts/mock-ollama.mjs')]);
    await waitForHttp(env.OLLAMA_BASE + '/api/tags', 'Mock Ollama', stack.mock, async (response) => {
      const data = await response.json();
      return data.models?.some((model) => model.name === 'mock:latest');
    });
    stack.turn = stack.launch('turn', binary('turn-server'));
    await waitFor(() => stack.turn.output.includes('TURN TLS / HTTPS listening'), 'TURN server');
    await stack.restartAgent();
    const demo = {
      signalingUrl: env.SIGNALING_URL, room: env.ROOM_ID, token, model: 'qwen2.5:7b', mock: true,
      stunUrls: env.STUN_URL, turnUrls: 'turn:127.0.0.1:3478?transport=udp,turn:127.0.0.1:3478?transport=tcp',
      turnUsername: env.TURN_USER, turnCredential: turnPassword,
    };
    stack.frontend = stack.launch('frontend', process.execPath, [vite, '--host', '127.0.0.1', '--port', '5173', '--strictPort', resolve(root, 'frontend')], { LOCAL_DEMO_CONFIG: JSON.stringify(demo) });
    await waitForHttp(stack.http + '/', 'frontend page', stack.frontend, async (response) => (await response.text()).includes('<div id="root">'));
    await waitForHttp(stack.http + '/__local/config', 'frontend configuration', stack.frontend, async (response) => {
      const config = await response.json();
      return config.mock === true && config.token === token && config.room === env.ROOM_ID;
    });
    const controlToken = randomBytes(24).toString('hex');
    stack.control = http.createServer((request, response) => {
      if (request.headers.authorization !== 'Bearer ' + controlToken) { response.statusCode = 404; response.end(); return; }
      if (request.method === 'GET' && request.url === '/status') {
        response.setHeader('Content-Type', 'application/json');
        response.end(JSON.stringify({ status: stopped ? 'stopping' : 'running', frontend: stack.http }));
      } else if (request.method === 'POST' && request.url === '/shutdown') {
        response.end('stopping');
        setTimeout(() => stack.stop().catch(console.error), 50);
      } else { response.statusCode = 404; response.end(); }
    });
    await new Promise((ready) => stack.control.listen(0, '127.0.0.1', ready));
    await writeFile(resolve(localDir, 'runtime.json'), JSON.stringify({ frontend: stack.http, mock: env.OLLAMA_BASE, settings: demo, control: 'http://127.0.0.1:' + stack.control.address().port + '/shutdown', controlToken }, null, 2), { mode: 0o600 });
    return stack;
  } catch (error) {
    for (const child of children) if (child.output) console.error(child.output.slice(-3000));
    await stack.stop();
    throw error;
  }
}

export async function stopLocalStack() {
  const file = await readFile(resolve(localDir, 'runtime.json'), 'utf8').catch((error) => {
    if (error.code === 'ENOENT') return null;
    throw error;
  });
  if (!file) return false;
  const runtime = JSON.parse(file);
  if (runtime.stopped) return false;
  const url = new URL(runtime.control);
  if (url.protocol !== 'http:' || url.hostname !== '127.0.0.1' || url.pathname !== '/shutdown') throw new Error('Invalid local control URL');
  const response = await fetch(url, { method: 'POST', headers: { Authorization: 'Bearer ' + runtime.controlToken }, signal: AbortSignal.timeout(5000) });
  if (!response.ok) throw new Error('Failed to stop the local stack');
  await waitFor(async () => {
    try { return JSON.parse(await readFile(resolve(localDir, 'runtime.json'), 'utf8')).stopped === true; }
    catch { return false; }
  }, 'local services to stop', 10000);
  return true;
}
