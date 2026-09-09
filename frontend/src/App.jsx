import React, { useEffect, useRef, useState } from 'react';
import { OllamaRemoteClient } from './webrtcClient.js';
import { readNdjson } from './ndjson.js';
import './style.css';

const local = ['localhost', '127.0.0.1', '[::1]'].includes(location.hostname);
const initialConfig = {
  signalingUrl: local ? 'ws://127.0.0.1:8080/ws' : 'wss://' + location.host + '/ws',
  room: 'my-room', token: '', model: 'qwen2.5:7b',
  stunUrls: '', turnUrls: '', turnUsername: '', turnCredential: '', forceRelay: false,
};
const stateLabels = { disconnected: '未连接', connecting: '正在连接', waiting: '等待家里的 Agent', connected: '已连接', reconnecting: '正在重新连接', error: '连接失败' };
const urls = (text) => text.split(/[\s,]+/).filter(Boolean);

export default function App() {
  const [config, setConfig] = useState(initialConfig);
  const [state, setState] = useState('disconnected');
  const [route, setRoute] = useState('');
  const [error, setError] = useState('');
  const [models, setModels] = useState([]);
  const [messages, setMessages] = useState([]);
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  const clientRef = useRef(null);
  const closingRef = useRef(Promise.resolve());
  const connectionActionRef = useRef(0);
  const abortRef = useRef(null);
  const sendingRef = useRef(false);
  const mountedRef = useRef(true);
  const bottomRef = useRef(null);
  const connected = state === 'connected';

  useEffect(() => {
    mountedRef.current = true;
    let cancelled = false;
    if (import.meta.env.DEV && local) {
      fetch('/__local/config').then((response) => response.ok ? response.json() : null).then((value) => {
        if (value && !cancelled) setConfig((previous) => ({ ...previous, ...value }));
      }).catch(() => {});
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

  const loadModels = async (client) => {
    try {
      const response = await client.fetch('/api/tags');
      if (!response.ok) throw new Error('无法读取模型列表：HTTP ' + response.status);
      const data = await response.json();
      if (client !== clientRef.current || !mountedRef.current) return;
      const names = (data.models || []).map((model) => model.name || model.model).filter(Boolean);
      setModels(names);
      setConfig((previous) => ({ ...previous, model: names.includes(previous.model) ? previous.model : names[0] || previous.model }));
    } catch (failure) {
      if (client === clientRef.current && mountedRef.current) setError(failure.message);
    }
  };

  const handleConnect = async () => {
    setError('');
    if (!/^[a-zA-Z0-9_-]{1,64}$/.test(config.room.trim()) || config.token.length < 16) {
      setError('房间号需为 1–64 个字母、数字、短横线或下划线，Token 至少 16 个字符');
      return;
    }
    const iceServers = [];
    if (urls(config.stunUrls).length) iceServers.push({ urls: urls(config.stunUrls) });
    if (urls(config.turnUrls).length) iceServers.push({ urls: urls(config.turnUrls), username: config.turnUsername, credential: config.turnCredential });
    if (config.forceRelay && !urls(config.turnUrls).length) {
      setError('仅使用中继时，需要填写 TURN 地址和凭据');
      return;
    }
    if (urls(config.turnUrls).length && (!config.turnUsername || !config.turnCredential)) {
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
      signalingUrl: config.signalingUrl.trim(), room: config.room.trim(), token: config.token,
      iceServers, forceRelay: config.forceRelay,
      onState: (next, detail) => {
        if (!mountedRef.current || client !== clientRef.current) return;
        setState(next);
        if (detail.error) setError(detail.error);
        if (next === 'connected') {
          setError('');
          loadModels(client);
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

  const handleDisconnect = () => {
    ++connectionActionRef.current;
    abortRef.current?.abort();
    const client = clientRef.current;
    clientRef.current = null;
    if (client) closingRef.current = client.disconnect();
    setState('disconnected');
    setError('');
    setRoute('');
  };

  const handleSend = async () => {
    const client = clientRef.current;
    if (!connected || !client || sendingRef.current || !input.trim()) return;
    sendingRef.current = true;
    setBusy(true);
    setError('');
    const controller = new AbortController();
    abortRef.current = controller;
    const user = { id: crypto.randomUUID(), role: 'user', content: input.trim() };
    const assistantId = crypto.randomUUID();
    const history = [...messages.filter((message) => !message.error), user].map(({ role, content }) => ({ role, content }));
    setMessages((previous) => [...previous, user, { id: assistantId, role: 'assistant', model: config.model, content: '' }]);
    setInput('');
    const updateAssistant = (change) => {
      if (mountedRef.current) setMessages((previous) => previous.map((message) => message.id === assistantId ? { ...message, ...change } : message));
    };
    try {
      const response = await client.fetch('/api/chat', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ model: config.model, messages: history, stream: true }), signal: controller.signal,
      });
      if (!response.ok) {
        const text = await response.text();
        let reason = text;
        try { reason = JSON.parse(text).error || text; } catch {}
        throw new Error(reason || 'HTTP ' + response.status);
      }
      if (!response.body) throw new Error('模型没有返回响应内容');
      let content = '';
      let completed = false;
      for await (const chunk of readNdjson(response.body)) {
        if (chunk.error) throw new Error(chunk.error);
        if (chunk.message?.content) {
          content += chunk.message.content;
          updateAssistant({ content });
        }
        if (chunk.done) completed = true;
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
        <div className={'connection-badge ' + (connected ? 'online' : '')} role="status"><span />{stateLabels[state]}{connected && route ? ' · ' + route : ''}</div>
      </header>
      {config.mock && <div className="mock-banner">当前使用本地 Mock Ollama，返回固定测试内容。</div>}
      <details className="settings" open={!connected}>
        <summary>连接设置</summary>
        <div className="settings-grid">
          <label className="wide">信令地址<input value={config.signalingUrl} onChange={update('signalingUrl')} placeholder="wss://chat.example.com/ws" /></label>
          <label>房间号<input value={config.room} onChange={update('room')} autoComplete="off" /></label>
          <label>Token<input type="password" value={config.token} onChange={update('token')} autoComplete="off" /></label>
          <label className="wide">STUN 地址（可选）<input value={config.stunUrls} onChange={update('stunUrls')} placeholder="stun:turn.example.com:3478" /></label>
          <label className="wide">TURN 地址（多个地址用逗号分隔）<input value={config.turnUrls} onChange={update('turnUrls')} placeholder="turns:turn.example.com:443?transport=tcp" /></label>
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
          <label>模型<input list="models" value={config.model} onChange={update('model')} disabled={busy} /></label>
          <datalist id="models">{models.map((model) => <option key={model} value={model} />)}</datalist>
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
