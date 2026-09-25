import React, { useEffect, useRef, useState } from 'react';
import { OllamaRemoteClient, safeRandomUUID } from './webrtcClient.js';
import { readNdjson } from './ndjson.js';
import { readSse } from './sse.js';
import './style.css';

const local = ['localhost', '127.0.0.1', '[::1]'].includes(location.hostname);
const initialConfig = {
  signalingUrl: local ? 'ws://127.0.0.1:8080/ws' : (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws',
  room: 'default', token: '', model: 'qwen2.5:7b', apiMode: 'ollama',
  stunUrls: '', turnUrls: '', turnUsername: '', turnCredential: '', forceRelay: false,
};
const stateLabels = { disconnected: '未连接', connecting: '正在连接', waiting: '等待家里的 Agent', connected: '已连接', reconnecting: '正在重新连接', error: '连接失败' };
const urls = (text) => text.split(/[\s,]+/).filter(Boolean);

function parseUrlConfig() {
  if (typeof window === 'undefined') return {};
  const params = new URLSearchParams(window.location.search);
  const hash = window.location.hash.startsWith('#')
    ? new URLSearchParams(window.location.hash.slice(1))
    : new URLSearchParams();
  const get = (key, ...aliases) => {
    for (const k of [key, ...aliases]) {
      if (hash.has(k)) return hash.get(k);
      if (params.has(k)) return params.get(k);
    }
    return null;
  };
  const cfg = {};
  const room = get('room', 'r');
  if (room) cfg.room = room;
  const token = get('token', 't');
  if (token) cfg.token = token;
  const signalingUrl = get('signalingUrl', 'signaling', 's', 'ws');
  if (signalingUrl) cfg.signalingUrl = signalingUrl;
  const model = get('model', 'm');
  if (model) cfg.model = model;
  const apiMode = get('apiMode');
  if (apiMode === 'openai' || apiMode === 'ollama') cfg.apiMode = apiMode;
  const stunUrls = get('stunUrls', 'stun');
  if (stunUrls) cfg.stunUrls = stunUrls;
  const turnUrls = get('turnUrls', 'turn');
  if (turnUrls) cfg.turnUrls = turnUrls;
  const turnUsername = get('turnUsername', 'turnUser', 'u');
  if (turnUsername) cfg.turnUsername = turnUsername;
  const turnCredential = get('turnCredential', 'turnPass', 'p');
  if (turnCredential) cfg.turnCredential = turnCredential;
  const forceRelay = get('forceRelay', 'relay');
  if (forceRelay !== null) cfg.forceRelay = forceRelay === 'true' || forceRelay === '1';
  const autoConnect = get('auto', 'autoConnect');
  if (autoConnect !== null) cfg.autoConnect = autoConnect === 'true' || autoConnect === '1';
  return cfg;
}

export default function App() {
  const [config, setConfig] = useState(initialConfig);
  const [state, setState] = useState('disconnected');
  const [route, setRoute] = useState('');
  const [error, setError] = useState('');
  const [models, setModels] = useState([]);
  const [loadingModels, setLoadingModels] = useState(false);
  const [customModelMode, setCustomModelMode] = useState(false);
  const [messages, setMessages] = useState([]);
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  const clientRef = useRef(null);
  const closingRef = useRef(Promise.resolve());
  const connectionActionRef = useRef(0);
  const modelLoadRef = useRef(0);
  const apiModeRef = useRef(initialConfig.apiMode);
  const abortRef = useRef(null);
  const sendingRef = useRef(false);
  const mountedRef = useRef(true);
  const bottomRef = useRef(null);
  const connected = state === 'connected';

  const doConnect = async (activeConfig) => {
    setError('');
    const c = activeConfig || config;
    apiModeRef.current = c.apiMode;
    if (!/^[a-zA-Z0-9_-]{1,64}$/.test(c.room.trim()) || c.token.length < 16) {
      setError('房间号需为 1–64 个字母、数字、短横线或下划线，Token 至少 16 个字符');
      return;
    }
    const iceServers = [];
    if (urls(c.stunUrls).length) iceServers.push({ urls: urls(c.stunUrls) });
    if (urls(c.turnUrls).length) iceServers.push({ urls: urls(c.turnUrls), username: c.turnUsername, credential: c.turnCredential });
    if (urls(c.turnUrls).length && (!c.turnUsername || !c.turnCredential)) {
      setError('请填写 TURN 用户名和密码');
      return;
    }
    const action = ++connectionActionRef.current;
    const previous = clientRef.current;
    clientRef.current = null;
    abortRef.current?.abort();
    setRoute('');
    setModels([]);
    setState('connecting');
    if (previous) closingRef.current = previous.disconnect();
    await closingRef.current;
    if (!mountedRef.current || action !== connectionActionRef.current) return;
    const client = new OllamaRemoteClient({
      signalingUrl: c.signalingUrl.trim(), room: c.room.trim(), token: c.token,
      iceServers, forceRelay: c.forceRelay,
      onState: (next, detail) => {
        if (!mountedRef.current || client !== clientRef.current) return;
        setState(next);
        if (detail.error) setError(detail.error);
        if (next === 'connected') {
          setError('');
          loadModels(client, apiModeRef.current);
          client.getConnectionInfo().then((info) => {
            if (client === clientRef.current && mountedRef.current) setRoute(info?.relay ? 'TURN 中继' : '直接连接');
          }).catch(() => {});
        }
      },
    });
    clientRef.current = client;
    try { await client.connect(); } catch (failure) {
      if (client === clientRef.current && mountedRef.current) setError(failure.message);
    }
  };

  const handleConnect = () => doConnect(config);

  useEffect(() => {
    mountedRef.current = true;
    let cancelled = false;
    const urlConfig = parseUrlConfig();
    let current = { ...initialConfig, ...urlConfig };
    if (Object.keys(urlConfig).length > 0) {
      setConfig((previous) => {
        current = { ...previous, ...urlConfig };
        return current;
      });
    }

    if (import.meta.env.DEV && local) {
      fetch('/__local/config').then((response) => response.ok ? response.json() : null).then((value) => {
        if (value && !cancelled) {
          setConfig((previous) => {
            current = { ...previous, ...value };
            return current;
          });
        }
      }).catch(() => {});
    }

    if (urlConfig.room && urlConfig.token && urlConfig.autoConnect !== false) {
      setTimeout(() => {
        if (mountedRef.current && state === 'disconnected') {
          doConnect(current);
        }
      }, 100);
    }

    const cleanup = () => {
      ++connectionActionRef.current;
      abortRef.current?.abort();
      const client = clientRef.current;
      clientRef.current = null;
      if (client) closingRef.current = client.disconnect();
    };
    window.addEventListener('pagehide', cleanup);
    return () => {
      cancelled = true;
      mountedRef.current = false;
      cleanup();
      window.removeEventListener('pagehide', cleanup);
    };
  }, []);

  useEffect(() => { bottomRef.current?.scrollIntoView({ block: 'end' }); }, [messages]);
  const update = (key) => (event) => setConfig((previous) => ({ ...previous, [key]: event.target.type === 'checkbox' ? event.target.checked : event.target.value }));

  const loadModels = async (client, apiMode = config.apiMode) => {
    if (!client) return;
    const load = ++modelLoadRef.current;
    setLoadingModels(true);
    try {
      const response = await client.fetch(apiMode === 'openai' ? '/v1/models' : '/api/tags');
      if (!response.ok) throw new Error('无法读取模型列表：HTTP ' + response.status);
      const data = await response.json();
      if (client !== clientRef.current || !mountedRef.current || load !== modelLoadRef.current) return;
      const names = (apiMode === 'openai' ? data.data || [] : data.models || [])
        .map((model) => apiMode === 'openai' ? model.id : model.name || model.model).filter(Boolean);
      setModels(names);
      if (names.length > 0) {
        setConfig((previous) => ({
          ...previous,
          model: names.includes(previous.model) ? previous.model : names[0],
        }));
      }
    } catch (failure) {
      if (client === clientRef.current && mountedRef.current && load === modelLoadRef.current) setError(failure.message);
    } finally {
      if (client === clientRef.current && mountedRef.current && load === modelLoadRef.current) setLoadingModels(false);
    }
  };

  const handleDisconnect = () => {
    ++connectionActionRef.current;
    ++modelLoadRef.current;
    abortRef.current?.abort();
    const client = clientRef.current;
    clientRef.current = null;
    if (client) closingRef.current = client.disconnect();
    setState('disconnected');
    setError('');
    setRoute('');
    setModels([]);
    setLoadingModels(false);
    setCustomModelMode(false);
  };

  const handleSend = async () => {
    const client = clientRef.current;
    if (!connected || !client || sendingRef.current || !input.trim()) return;
    sendingRef.current = true;
    setBusy(true);
    setError('');
    const controller = new AbortController();
    abortRef.current = controller;
    const user = { id: safeRandomUUID(), role: 'user', content: input.trim() };
    const assistantId = safeRandomUUID();
    const history = [...messages.filter((message) => !message.error), user].map(({ role, content }) => ({ role, content }));
    setMessages((previous) => [...previous, user, { id: assistantId, role: 'assistant', model: config.model, content: '' }]);
    setInput('');
    const updateAssistant = (change) => {
      if (mountedRef.current) setMessages((previous) => previous.map((message) => message.id === assistantId ? { ...message, ...change } : message));
    };
    try {
      const response = await client.fetch(config.apiMode === 'openai' ? '/v1/chat/completions' : '/api/chat', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ model: config.model, messages: history, stream: true }), signal: controller.signal,
      });
      if (!response.ok) {
        const text = await response.text();
        let reason = text;
        try {
          const parsed = JSON.parse(text);
          reason = parsed.error?.message || parsed.error || text;
        } catch {}
        throw new Error(typeof reason === 'string' ? reason : 'HTTP ' + response.status);
      }
      if (!response.body) throw new Error('模型没有返回响应内容');
      let content = '';
      let completed = false;
      for await (const chunk of (config.apiMode === 'openai' ? readSse(response.body) : readNdjson(response.body))) {
        if (chunk.error) throw new Error(chunk.error?.message || chunk.error);
        const delta = config.apiMode === 'openai' ? chunk.choices?.[0]?.delta?.content : chunk.message?.content;
        if (delta) {
          content += delta;
          updateAssistant({ content });
        }
        if (chunk.done || (config.apiMode === 'openai' && chunk.choices?.[0]?.finish_reason)) completed = true;
      }
      if (!completed) throw new Error('模型响应在完成前中断');
    } catch (failure) {
      if (failure.name === 'AbortError') updateAssistant({ note: '已停止生成' });
      else {
        updateAssistant({ error: failure.message });
        if (mountedRef.current) setError(failure.message);
      }
    } finally {
      if (abortRef.current === controller) abortRef.current = null;
      sendingRef.current = false;
      if (mountedRef.current) setBusy(false);
    }
  };

  return (
    <main className="app">
      <header className="app-header">
        <div><span className="eyebrow">YOUR MODEL, AT HOME</span><h1>远程 Ollama</h1><p>在浏览器里，与家里的模型对话。</p></div>
        <div className="header-actions">
          <a href="/setup" target="_blank" rel="noreferrer" className="setup-link" title="查看配置与接入指南">⚙️ 配置与接入</a>
          <div className={'connection-badge ' + (connected ? 'online' : '')} role="status"><span />{stateLabels[state]}{connected && route ? ' · ' + route : ''}</div>
        </div>
      </header>
      {config.mock && <div className="mock-banner">当前使用本地 Mock Ollama，返回固定测试内容。</div>}
      <details className="settings" open={!connected}>
        <summary>连接设置</summary>
        <div className="settings-grid">
          <label>聊天接口<select value={config.apiMode} onChange={(event) => {
            const apiMode = event.target.value;
            apiModeRef.current = apiMode;
            setConfig((previous) => ({ ...previous, apiMode }));
            setModels([]);
            if (clientRef.current) loadModels(clientRef.current, apiMode);
          }} disabled={busy}>
            <option value="ollama">Ollama 原生</option>
            <option value="openai">OpenAI 兼容</option>
          </select></label>
          <label className="wide">信令地址<input value={config.signalingUrl} onChange={update('signalingUrl')} placeholder="wss://chat.example.com/ws" /></label>
          <label>房间号<input value={config.room} onChange={update('room')} autoComplete="off" /></label>
          <label>Token<input type="password" value={config.token} onChange={update('token')} autoComplete="off" /></label>
          <label className="wide">STUN 地址（可选，信令服务器会自动下发）<input value={config.stunUrls} onChange={update('stunUrls')} placeholder="stun:turn.example.com:3478" /></label>
          <label className="wide">TURN 地址（可选，信令服务器会自动下发；多个地址用逗号分隔）<input value={config.turnUrls} onChange={update('turnUrls')} placeholder="turns:turn.example.com:443?transport=tcp" /></label>
          <label>TURN 用户名<input value={config.turnUsername} onChange={update('turnUsername')} autoComplete="off" /></label>
          <label>TURN 密码<input type="password" value={config.turnCredential} onChange={update('turnCredential')} autoComplete="off" /></label>
          <label className="check wide"><input type="checkbox" checked={config.forceRelay} onChange={update('forceRelay')} />仅使用中继，用于受限网络或中继测试</label>
        </div>
        <div className="connection-actions">
          <button onClick={handleConnect} disabled={state === 'connecting'}>{connected ? '应用设置并重新连接' : '连接'}</button>
          {state !== 'disconnected' && <button className="secondary" onClick={handleDisconnect}>断开</button>}
        </div>
      </details>
      <section className="chat" aria-label="聊天">
        <div className="chat-toolbar">
          <div className="chat-toolbar-left">
            <label htmlFor="model-select">模型</label>
            {customModelMode ? (
              <div className="custom-model-box">
                <input
                  id="model-select"
                  value={config.model}
                  onChange={update('model')}
                  placeholder="输入模型名称，如 llama3:8b"
                  disabled={busy}
                  autoFocus
                />
                <button
                  type="button"
                  className="text-button"
                  onClick={() => {
                    setCustomModelMode(false);
                    if (models.length > 0 && !models.includes(config.model)) {
                      setConfig((prev) => ({ ...prev, model: models[0] }));
                    }
                  }}
                  title="返回模型下拉列表"
                >
                  列表选择
                </button>
              </div>
            ) : (
              <select
                id="model-select"
                value={config.model}
                onChange={(e) => {
                  if (e.target.value === '__custom__') {
                    setCustomModelMode(true);
                  } else {
                    setConfig((prev) => ({ ...prev, model: e.target.value }));
                  }
                }}
                disabled={busy || !connected || loadingModels}
              >
                {loadingModels ? (
                  <option value="">正在获取模型列表...</option>
                ) : !connected ? (
                  <option value={config.model}>{config.model ? `${config.model} (连接后自动获取)` : '未连接 (连接后自动获取)'}</option>
                ) : models.length === 0 ? (
                  <>
                    <option value={config.model}>{config.model ? `${config.model} (未获取到模型)` : '未发现模型'}</option>
                    <option value="__custom__">✏️ 自定义输入模型...</option>
                  </>
                ) : (
                  <>
                    {models.map((m) => (
                      <option key={m} value={m}>{m}</option>
                    ))}
                    {!models.includes(config.model) && config.model && (
                      <option value={config.model}>{config.model} (自定义)</option>
                    )}
                    <option value="__custom__">✏️ 自定义输入模型...</option>
                  </>
                )}
              </select>
            )}
            {connected && (
              <button
                type="button"
                className="text-button refresh-button"
                onClick={() => clientRef.current && loadModels(clientRef.current)}
                disabled={loadingModels || busy}
                title="重新获取可用模型列表"
              >
                {loadingModels ? '获取中…' : '🔄 刷新'}
              </button>
            )}
          </div>
          <button className="text-button" onClick={() => setMessages([])} disabled={busy || !messages.length}>清空对话</button>
        </div>
        <div className="messages" aria-live="polite">
          {!messages.length && <div className="empty-state"><span>家里的算力，随时可用。</span><p>{connected ? '模型已就绪，发送第一条消息。' : '连接家里的 Agent 后开始聊天。'}</p></div>}
          {messages.map((message) => <article key={message.id} className={'message ' + message.role}>
            <div className="message-role">{message.role === 'user' ? '你' : message.model}</div>
            <div className="message-content">{message.content || (busy && message.role === 'assistant' ? '正在思考…' : '')}</div>
            {message.error && <div className="message-error">{message.error}</div>}
            {message.note && <div className="message-note">{message.note}</div>}
          </article>)}
          <div ref={bottomRef} />
        </div>
        {error && <div className="error" role="alert">{error}</div>}
        <div className="composer">
          <textarea value={input} onChange={(event) => setInput(event.target.value)} placeholder={connected ? '输入消息，Enter 发送，Shift + Enter 换行' : '连接后开始对话'} disabled={!connected} rows={3} onKeyDown={(event) => {
            if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) { event.preventDefault(); handleSend(); }
          }} />
          {busy ? <button className="secondary" onClick={() => abortRef.current?.abort()}>停止生成</button> : <button disabled={!connected || !input.trim()} onClick={handleSend}>发送</button>}
        </div>
      </section>
    </main>
  );
}
