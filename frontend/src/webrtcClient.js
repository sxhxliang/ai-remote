const encoder = new TextEncoder();
const MAX_FRAME = 16 * 1024;
const CHUNK_BYTES = 8 * 1024;
const MAX_REQUEST = 8 * 1024 * 1024;

const encodeBytes = (bytes) => btoa(String.fromCharCode(...bytes));
const decodeBytes = (text) => Uint8Array.from(atob(text), (char) => char.charCodeAt(0));
const abortError = () => new DOMException('请求已取消', 'AbortError');
const DEFAULT_STUN_URLS = ['stun:stun.miwifi.com:3478', 'stun:stun.cloudflare.com:3478'];
const MAX_ICE_ENTRIES = 8;

// ICE servers pushed by the signaling server in `ready`, after token authentication.
// Anything a browser would reject is dropped, because one bad entry makes RTCPeerConnection throw.
export function sanitizeIceServers(value) {
  if (!Array.isArray(value)) return [];
  return value.slice(0, MAX_ICE_ENTRIES).flatMap((server) => {
    const urls = [].concat(server?.urls ?? []).filter((url) => typeof url === 'string' && /^(stun|turns?):/.test(url)).slice(0, MAX_ICE_ENTRIES);
    if (!urls.length) return [];
    if (!urls.some((url) => url.startsWith('turn'))) return [{ urls }];
    if (typeof server.username !== 'string' || typeof server.credential !== 'string') return [];
    return [{ urls, username: server.username, credential: server.credential }];
  });
}

// Local settings first, then server-provided servers without duplicate URLs.
// Public STUN is only a last resort when neither side configured anything.
export function mergeIceServers(local = [], remote = []) {
  const seen = new Set();
  const merged = [];
  for (const server of [...local, ...remote]) {
    const urls = [].concat(server.urls).filter((url) => !seen.has(url));
    urls.forEach((url) => seen.add(url));
    if (urls.length) merged.push({ ...server, urls });
  }
  return merged.length ? merged : [{ urls: DEFAULT_STUN_URLS }];
}

const hasRelay = (servers) => servers.some((server) => [].concat(server.urls).some((url) => /^turns?:/.test(url)));

export function safeRandomUUID() {
  if (typeof crypto !== 'undefined') {
    if (typeof crypto.randomUUID === 'function') {
      return crypto.randomUUID();
    }
    if (typeof crypto.getRandomValues === 'function') {
      const bytes = new Uint8Array(16);
      crypto.getRandomValues(bytes);
      bytes[6] = (bytes[6] & 0x0f) | 0x40;
      bytes[8] = (bytes[8] & 0x3f) | 0x80;
      const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
      return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
    }
  }
  return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (c) => {
    const r = (Math.random() * 16) | 0;
    const v = c === 'x' ? r : (r & 0x3) | 0x8;
    return v.toString(16);
  });
}

async function readRequestBody(body, signal) {
  if (!body) return null;
  const reader = body.getReader();
  const decoder = new TextDecoder('utf-8', { fatal: true });
  const parts = [];
  let size = 0;
  const cancel = () => { reader.cancel(signal.reason).catch(() => {}); };
  signal.addEventListener('abort', cancel, { once: true });
  try {
    if (signal.aborted) throw signal.reason;
    while (true) {
      const { value, done } = await reader.read();
      if (signal.aborted) throw signal.reason;
      if (done) break;
      size += value.byteLength;
      if (size > MAX_REQUEST) throw new Error('请求超过 8 MiB，请缩短对话或减小附件');
      parts.push(decoder.decode(value, { stream: true }));
    }
    parts.push(decoder.decode());
    return parts.join('');
  } catch (error) {
    reader.cancel(error).catch(() => {});
    throw error;
  } finally {
    signal.removeEventListener('abort', cancel);
    reader.releaseLock();
  }
}

export class OllamaRemoteClient {
  constructor({ signalingUrl, room, token, iceServers = [], forceRelay = false, onState = () => {}, connectTimeoutMs = 30000, requestTimeoutMs = 600000, autoReconnect = true } = {}) {
    Object.assign(this, { signalingUrl, room, token, iceServers, forceRelay, onState, connectTimeoutMs, requestTimeoutMs, autoReconnect });
    this.pc = null;
    this.dc = null;
    this.ws = null;
    this.pending = new Map();
    this.epoch = 0;
    this.desired = false;
    this.everConnected = false;
    this.retry = 0;
    this.session = null;
    this.serverIceServers = [];
    this.fetch = this.ollamaFetch.bind(this);
  }

  _state(state, detail = {}) {
    this.state = state;
    this.onState(state, detail);
  }

  connect() {
    this.desired = true;
    if (this.dc?.readyState === 'open') return Promise.resolve();
    if (this.connectWait) return this.connectWait.promise;
    clearTimeout(this.retryTimer);
    if (this.ws) return this._connectionPromise();
    return this._connectAttempt();
  }

  _connectionPromise() {
    let resolve;
    let reject;
    const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
    this.connectWait = { promise, resolve, reject };
    return promise;
  }

  _connectAttempt() {
    const epoch = ++this.epoch;
    const promise = this._connectionPromise();
    this._state('connecting');
    const open = () => {
      if (epoch !== this.epoch || !this.desired) return;
      try {
        const url = new URL(this.signalingUrl);
        if (!['ws:', 'wss:'].includes(url.protocol)) throw new Error('信令地址必须使用 ws:// 或 wss://');
        url.searchParams.set('room', this.room);
        url.searchParams.set('role', 'browser');
        url.searchParams.set('token', this.token);
        const ws = new WebSocket(url.href);
        this.ws = ws;
        this.signalQueue = Promise.resolve();
        ws.onmessage = (event) => {
          this.signalQueue = this.signalQueue.then(async () => {
            if (epoch !== this.epoch) return;
            await this._signalMessage(JSON.parse(event.data));
          }).catch((error) => { if (epoch === this.epoch) this._fail(error); });
        };
        ws.onerror = () => {
          if (epoch === this.epoch) this._fail(new Error('信令连接失败，请检查地址、房间、Token，以及房间是否已被占用'));
        };
        ws.onclose = () => {
          if (epoch === this.epoch) this._fail(new Error('信令连接已关闭'));
        };
        ws.onopen = () => {
          if (epoch !== this.epoch) return;
          this.heartbeat = setInterval(() => {
            try { this._signal({ type: 'ping' }); } catch (error) { this._fail(error); }
          }, 20000);
        };
        this._startConnectTimeout();
      } catch (error) {
        this._fail(error);
      }
    };
    if (this.closing) this.closing.then(open);
    else open();
    return promise;
  }

  _startConnectTimeout() {
    clearTimeout(this.connectTimer);
    this.connectTimer = setTimeout(() => this._fail(new Error('连接超时，请确认家里的 Agent 在线及中继配置正确')), this.connectTimeoutMs);
  }

  _signal(value) {
    if (this.ws?.readyState !== WebSocket.OPEN) throw new Error('信令未连接');
    this.ws.send(JSON.stringify(value));
  }

  async _signalMessage(message) {
    if (message.type === 'ready') {
      if (message.protocol !== 1) throw new Error('不兼容的信令协议版本');
      this.serverIceServers = sanitizeIceServers(message.iceServers);
      if (message.peerOnline) await this._negotiate();
      else this._state('waiting');
    } else if (message.type === 'peer-joined') {
      await this._negotiate();
    } else if (['peer-left', 'peer-unavailable'].includes(message.type)) {
      this._closePeer(new Error('家里的 Agent 已离线'));
      this._state('waiting');
      this._startConnectTimeout();
    } else if (message.type === 'error') {
      throw new Error(message.message || '信令请求被拒绝');
    } else if (message.session === this.session) {
      if (message.type === 'answer') {
        const pc = this.pc;
        if (!pc || pc.remoteDescription) return;
        await pc.setRemoteDescription({ type: 'answer', sdp: message.sdp });
        if (pc !== this.pc) return;
        for (const candidate of this.remoteCandidates.splice(0)) await pc.addIceCandidate(candidate);
      } else if (message.type === 'ice' && message.candidate) {
        if (this.pc?.remoteDescription) await this.pc.addIceCandidate(message.candidate);
        else {
          if (this.remoteCandidates.length >= 128) throw new Error('ICE 候选数量过多');
          this.remoteCandidates.push(message.candidate);
        }
      } else if (message.type === 'hangup') {
        throw new Error('家里的 Agent 结束了连接');
      }
    }
  }

  async _negotiate() {
    if (this.pc) return;
    const iceServers = mergeIceServers(this.iceServers, this.serverIceServers);
    if (this.forceRelay && !hasRelay(iceServers)) throw new Error('仅使用中继需要 TURN：请在连接设置中填写 TURN 地址，或在信令服务器配置 TURN_URL');
    const session = safeRandomUUID();
    this.session = session;
    this.remoteCandidates = [];
    const localCandidates = [];
    let offered = false;
    const pc = new RTCPeerConnection({ iceServers, iceTransportPolicy: this.forceRelay ? 'relay' : 'all' });
    this.pc = pc;
    const dc = pc.createDataChannel('ollama', { ordered: true });
    this.dc = dc;
    dc.bufferedAmountLowThreshold = 64 * 1024;
    dc.onopen = () => {
      if (pc !== this.pc) return;
      clearTimeout(this.connectTimer);
      this.everConnected = true;
      this.retry = 0;
      const wait = this.connectWait;
      this.connectWait = null;
      this._state('connected');
      wait?.resolve();
    };
    dc.onmessage = (event) => { if (pc === this.pc) this._handleMessage(event.data); };
    dc.onerror = () => { if (pc === this.pc) this._fail(new Error('数据通道发生错误')); };
    dc.onclose = () => { if (pc === this.pc) this._fail(new Error('数据通道已关闭')); };
    pc.onconnectionstatechange = () => {
      if (pc !== this.pc) return;
      if (['failed', 'closed'].includes(pc.connectionState)) this._fail(new Error('WebRTC 连接失败'));
      else if (pc.connectionState === 'disconnected') {
        clearTimeout(this.disconnectTimer);
        this.disconnectTimer = setTimeout(() => {
          if (pc === this.pc && pc.connectionState === 'disconnected') this._fail(new Error('WebRTC 连接中断'));
        }, 5000);
      } else if (pc.connectionState === 'connected') clearTimeout(this.disconnectTimer);
    };
    pc.onicecandidate = (event) => {
      if (!event.candidate || pc !== this.pc) return;
      const message = { type: 'ice', session, candidate: event.candidate.toJSON() };
      if (!offered) localCandidates.push(message);
      else {
        try { this._signal(message); } catch (error) { this._fail(error); }
      }
    };
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    if (pc !== this.pc) return;
    this._signal({ type: 'offer', session, sdp: offer.sdp });
    offered = true;
    for (const candidate of localCandidates) this._signal(candidate);
  }

  _closePeer(error) {
    const pc = this.pc;
    const dc = this.dc;
    this.pc = null;
    this.dc = null;
    this.session = null;
    clearTimeout(this.disconnectTimer);
    for (const entry of [...this.pending.values()]) this._rejectEntry(entry, error);
    if (dc) { dc.onopen = dc.onclose = dc.onerror = dc.onmessage = null; dc.close(); }
    pc?.close();
  }

  _closeSignaling() {
    const ws = this.ws;
    this.ws = null;
    if (!ws) return this.closing || Promise.resolve();
    ws.onopen = ws.onmessage = ws.onerror = ws.onclose = null;
    const closed = new Promise((resolve) => {
      const finish = () => { clearTimeout(timer); ws.onclose = null; resolve(); };
      const timer = setTimeout(finish, 2000);
      ws.onclose = finish;
      if (ws.readyState === 3) finish();
      else { try { ws.close(); } catch { finish(); } }
    });
    this.closing = closed;
    closed.then(() => { if (this.closing === closed) this.closing = null; });
    return closed;
  }

  _fail(error) {
    ++this.epoch;
    clearTimeout(this.connectTimer);
    clearInterval(this.heartbeat);
    const closing = this._closeSignaling();
    this._closePeer(error);
    const wait = this.connectWait;
    this.connectWait = null;
    wait?.reject(error);
    this._state('error', { error: error.message });
    if (this.desired && this.everConnected && this.autoReconnect) {
      clearTimeout(this.retryTimer);
      const delay = Math.min(1000 * 2 ** this.retry++, 10000);
      this._state('reconnecting', { error: error.message });
      this.retryTimer = setTimeout(() => {
        if (this.desired) this._connectAttempt().catch(() => {});
      }, delay);
    }
    return closing;
  }

  disconnect() {
    this.desired = false;
    clearTimeout(this.retryTimer);
    try { if (this.session) this._signal({ type: 'hangup', session: this.session }); } catch {}
    const closing = this._fail(new Error('连接已断开'));
    this.everConnected = false;
    this.retry = 0;
    this._state('disconnected');
    return closing;
  }

  async getConnectionInfo() {
    if (!this.pc) return null;
    const stats = await this.pc.getStats();
    let pair;
    stats.forEach((item) => {
      if (item.type === 'transport' && item.selectedCandidatePairId) pair = stats.get(item.selectedCandidatePairId);
    });
    if (!pair) stats.forEach((item) => { if (item.type === 'candidate-pair' && item.nominated && item.state === 'succeeded') pair = item; });
    const local = pair && stats.get(pair.localCandidateId);
    const remote = pair && stats.get(pair.remoteCandidateId);
    return { relay: local?.candidateType === 'relay' || remote?.candidateType === 'relay', local, remote };
  }

  async _sendFrame(frame, signal) {
    const dc = this.dc;
    if (signal?.aborted) throw abortError();
    if (dc?.readyState !== 'open') throw new Error('尚未连接到家里的 Agent');
    const text = JSON.stringify(frame);
    if (encoder.encode(text).length > MAX_FRAME) throw new Error('数据帧过大');
    if (dc.bufferedAmount > 256 * 1024) {
      await new Promise((resolve, reject) => {
        const cleanup = () => {
          clearTimeout(timer);
          dc.removeEventListener('bufferedamountlow', writable);
          dc.removeEventListener('close', closed);
          signal?.removeEventListener('abort', aborted);
        };
        const writable = () => { cleanup(); resolve(); };
        const closed = () => { cleanup(); reject(new Error('数据通道已关闭')); };
        const aborted = () => { cleanup(); reject(abortError()); };
        const timer = setTimeout(() => { cleanup(); reject(new Error('发送数据超时')); }, 15000);
        dc.addEventListener('bufferedamountlow', writable);
        dc.addEventListener('close', closed);
        signal?.addEventListener('abort', aborted, { once: true });
        if (signal?.aborted) aborted();
        else if (dc.readyState !== 'open') closed();
        else if (dc.bufferedAmount <= 256 * 1024) writable();
      });
    }
    if (signal?.aborted) throw abortError();
    if (dc !== this.dc || dc.readyState !== 'open') throw new Error('数据通道已关闭');
    dc.send(text);
  }

  _control(frame) {
    this._sendFrame(frame).catch((error) => { if (this.desired && this.dc) this._fail(error); });
  }

  _cleanupEntry(entry, reason = abortError()) {
    this.pending.delete(entry.id);
    clearTimeout(entry.timer);
    entry.signal.removeEventListener('abort', entry.abort);
    entry.upload.abort(reason);
  }

  _rejectEntry(entry, error) {
    if (!this.pending.has(entry.id)) return;
    this._cleanupEntry(entry, error);
    entry.reject(error);
    entry.controller.error(error);
    entry.queue.length = 0;
  }

  _drain(entry) {
    if (entry.readRequested && entry.queue.length) {
      const chunk = entry.queue.shift();
      entry.readRequested = false;
      entry.controller.enqueue(chunk.bytes);
      this._control({ type: 'ack', id: entry.id, seq: chunk.seq });
    }
    if (entry.ended && entry.queue.length === 0) {
      entry.controller.close();
      this._cleanupEntry(entry);
    }
  }

  _handleMessage(raw) {
    try {
      if (typeof raw !== 'string' || raw.length > MAX_FRAME) throw new Error('无效的响应帧');
      const frame = JSON.parse(raw);
      const entry = this.pending.get(frame.id);
      if (!entry) {
        if (frame.type === 'error' && !frame.id) throw new Error(frame.message || '协议错误');
        return;
      }
      if (frame.type === 'response') {
        if (entry.responded) throw new Error('重复的响应头');
        if (!Number.isInteger(frame.status) || frame.status < 200 || frame.status > 599) throw new Error('无效的 HTTP 状态');
        entry.responded = true;
        entry.noBody = [204, 205, 304].includes(frame.status);
        entry.resolve(new Response(entry.noBody ? null : entry.stream, { status: frame.status, headers: frame.headers }));
      } else if (frame.type === 'chunk') {
        if (!entry.responded || entry.noBody || frame.seq !== entry.nextSeq++) throw new Error('响应分片顺序错误');
        const bytes = decodeBytes(frame.data);
        if (bytes.length > CHUNK_BYTES || entry.queue.length >= 8) throw new Error('响应缓冲区超限');
        entry.queue.push({ bytes, seq: frame.seq });
        this._drain(entry);
      } else if (frame.type === 'done') {
        if (!entry.responded) throw new Error('缺少响应头');
        entry.ended = true;
        this._drain(entry);
      } else if (frame.type === 'error') {
        if (!entry.responded && frame.status >= 400 && frame.status <= 599 && frame.status !== 499) {
          entry.resolve(new Response(JSON.stringify({ error: frame.message }), { status: frame.status, headers: { 'Content-Type': 'application/json' } }));
          entry.controller.close();
          this._cleanupEntry(entry);
        } else this._rejectEntry(entry, new Error(frame.message || '远端请求失败'));
      } else throw new Error('未知响应类型');
    } catch (error) {
      this._fail(error);
    }
  }

  async ollamaFetch(input, init = {}) {
    if (this.dc?.readyState !== 'open') throw new Error('尚未连接到家里的 Agent');
    const resource = typeof input === 'string' && input.startsWith('/') ? 'http://ollama.local' + input : input;
    const request = new Request(resource, init);
    if (request.signal.aborted) throw abortError();
    const url = new URL(request.url);
    if (!['http:', 'https:'].includes(url.protocol)) throw new Error('不支持的请求 URL');
    if (this.pending.size >= 4) throw new Error('同时最多支持 4 个请求');
    const id = safeRandomUUID();
    const channel = this.dc;
    let resolve;
    let reject;
    const response = new Promise((yes, no) => { resolve = yes; reject = no; });
    const entry = { id, resolve, reject, signal: request.signal, upload: new AbortController(), queue: [], nextSeq: 0, readRequested: false, ended: false, responded: false };
    entry.stream = new ReadableStream({
      start: (controller) => { entry.controller = controller; },
      pull: () => { entry.readRequested = true; this._drain(entry); },
      cancel: () => {
        if (this.pending.has(id)) {
          this._cleanupEntry(entry);
          this._control({ type: 'cancel', id });
        }
        entry.queue.length = 0;
      },
    }, { highWaterMark: 0 });
    entry.abort = () => {
      this._control({ type: 'cancel', id });
      this._rejectEntry(entry, abortError());
    };
    this.pending.set(id, entry);
    request.signal.addEventListener('abort', entry.abort, { once: true });
    entry.timer = setTimeout(() => {
      this._control({ type: 'cancel', id });
      this._rejectEntry(entry, new Error('请求超时'));
    }, this.requestTimeoutMs);
    if (request.signal.aborted) entry.abort();
    (async () => {
      const body = await readRequestBody(request.body, entry.upload.signal);
      if (!this.pending.has(id)) return;
      if (channel !== this.dc) throw new Error('连接已更换，请重新发送请求');
      const packet = { id, method: request.method, path: url.pathname + url.search, headers: Object.fromEntries(request.headers), body };
      const bytes = encoder.encode(JSON.stringify(packet));
      if (bytes.length > MAX_REQUEST) throw new Error('请求超过 8 MiB，请缩短对话或减小附件');
      if (bytes.length <= MAX_FRAME) await this._sendFrame(packet, entry.upload.signal);
      else {
        let seq = 0;
        for (let offset = 0; offset < bytes.length && this.pending.has(id); offset += CHUNK_BYTES) {
          await this._sendFrame({ type: 'request-fragment', id, seq: seq++, data: encodeBytes(bytes.subarray(offset, offset + CHUNK_BYTES)), done: offset + CHUNK_BYTES >= bytes.length }, entry.upload.signal);
        }
      }
    })().catch((error) => {
      if (!this.pending.has(id)) return;
      this._rejectEntry(entry, error);
      if (this.dc?.readyState === 'open') this._control({ type: 'cancel', id });
    });
    return response;
  }

  request(method, path, { headers = {}, body, onChunk = () => {}, signal } = {}) {
    const controller = new AbortController();
    const abort = () => controller.abort();
    signal?.addEventListener('abort', abort, { once: true });
    if (signal?.aborted) abort();
    const done = (async () => {
      try {
        const response = await this.fetch(path, { method, headers, body, signal: controller.signal });
        if (!response.ok) {
          const text = await response.text();
          let message = text;
          try { message = JSON.parse(text).error || text; } catch {}
          throw new Error(message || 'HTTP ' + response.status);
        }
        if (!response.body) return;
        const reader = response.body.getReader();
        const decoder = new TextDecoder();
        try {
          while (true) {
            const { value, done } = await reader.read();
            if (done) break;
            const text = decoder.decode(value, { stream: true });
            if (text) onChunk(text);
          }
          const tail = decoder.decode();
          if (tail) onChunk(tail);
        } finally { reader.releaseLock(); }
      } catch (error) {
        controller.abort();
        throw error;
      } finally { signal?.removeEventListener('abort', abort); }
    })();
    return { done, cancel: abort };
  }
}
