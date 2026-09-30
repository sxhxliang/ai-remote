import React, { useEffect, useRef, useState, useMemo } from 'react';
import { OllamaRemoteClient, safeRandomUUID } from './webrtcClient.js';
import { readNdjson } from './ndjson.js';
import { readSse } from './sse.js';
import {
  loadAgents,
  saveAgents,
  loadActiveAgentId,
  saveActiveAgentId,
  createDefaultAgent,
  generateSecureToken,
  generateServerEnvConfig,
  generateAgentCommands,
  getAgentAccessUrl,
  loadChatHistory,
  saveChatHistory,
  getDefaultSignalingUrl,
} from './storage.js';
import './style.css';

const stateLabels = {
  disconnected: '未连接',
  connecting: '正在连接',
  waiting: '等待 Agent 上线',
  connected: '已连接',
  reconnecting: '重连中',
  error: '连接异常',
};

const urls = (text) => (text || '').split(/[\s,]+/).filter(Boolean);

function parseCurrentRoute() {
  if (typeof window === 'undefined') return { routeRoom: null, routeToken: null };
  const hash = window.location.hash || '';
  const search = window.location.search || '';

  // 1. Match #/agent/:roomId or #/agent/:roomId?token=...
  const agentHashMatch = hash.match(/^#\/agent\/([^?#]+)(?:\?(.*))?$/);
  if (agentHashMatch) {
    const routeRoom = decodeURIComponent(agentHashMatch[1]);
    const query = new URLSearchParams(agentHashMatch[2] || '');
    return {
      routeRoom,
      routeToken: query.get('token') || query.get('t'),
      autoConnect: query.get('auto') !== 'false',
    };
  }

  // 2. Match legacy #room=...&token=... or ?room=...&token=...
  const hashParams = hash.startsWith('#') ? new URLSearchParams(hash.slice(1)) : new URLSearchParams();
  const searchParams = new URLSearchParams(search);
  const get = (key, ...aliases) => {
    for (const k of [key, ...aliases]) {
      if (hashParams.has(k)) return hashParams.get(k);
      if (searchParams.has(k)) return searchParams.get(k);
    }
    return null;
  };

  const room = get('room', 'r');
  const token = get('token', 't');
  if (room) {
    return {
      routeRoom: room,
      routeToken: token,
      autoConnect: get('auto', 'autoConnect') !== 'false',
    };
  }

  return { routeRoom: null, routeToken: null, autoConnect: true };
}

export default function App() {
  const [agents, setAgents] = useState(() => loadAgents());
  const [activeAgentId, setActiveAgentId] = useState(() => {
    const saved = loadActiveAgentId();
    const list = loadAgents();
    return list.some((a) => a.id === saved) ? saved : list[0]?.id || null;
  });

  const activeAgent = useMemo(() => {
    return agents.find((a) => a.id === activeAgentId) || agents[0] || null;
  }, [agents, activeAgentId]);

  const [connectionStates, setConnectionStates] = useState({});
  const [routesInfo, setRoutesInfo] = useState({});
  const [error, setError] = useState('');
  const [modelsMap, setModelsMap] = useState({});
  const [loadingModels, setLoadingModels] = useState(false);
  const [customModelMode, setCustomModelMode] = useState(false);

  // Chat messages per agent
  const [chatHistory, setChatHistory] = useState(() => loadChatHistory());
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);

  // UI Modals
  const [showEditModal, setShowEditModal] = useState(false);
  const [editingAgent, setEditingAgent] = useState(null);
  const [showTokensModal, setShowTokensModal] = useState(false);
  const [copiedKey, setCopiedKey] = useState('');
  const [sidebarOpen, setSidebarOpen] = useState(true);

  // References
  const clientRef = useRef(null);
  const closingRef = useRef(Promise.resolve());
  const connectionActionRef = useRef(0);
  const modelLoadRef = useRef(0);
  const abortRef = useRef(null);
  const sendingRef = useRef(false);
  const mountedRef = useRef(true);
  const bottomRef = useRef(null);

  const activeState = (activeAgent && connectionStates[activeAgent.id]) || 'disconnected';
  const activeRoute = activeAgent && routesInfo[activeAgent.id];
  const activeModels = (activeAgent && modelsMap[activeAgent.id]) || [];
  const currentMessages = (activeAgent && chatHistory[activeAgent.id]) || [];
  const isConnected = activeState === 'connected';

  // Connect WebRTC to a specific agent
  const doConnect = async (agentToConnect) => {
    const target = agentToConnect || activeAgent;
    if (!target) return;

    setError('');
    const room = (target.room || '').trim();
    const token = (target.token || '').trim();

    if (!/^[a-zA-Z0-9_-]{1,64}$/.test(room) || token.length < 16) {
      setError(`Agent [${target.name || room}] 房间号需为 1–64 位字母/数字/下划线/中划线，Token 至少 16 位`);
      return;
    }

    const iceServers = [];
    if (urls(target.stunUrls).length) iceServers.push({ urls: urls(target.stunUrls) });
    if (urls(target.turnUrls).length) {
      if (!target.turnUsername || !target.turnCredential) {
        setError('请填写 TURN 认证用户名与密码');
        return;
      }
      iceServers.push({
        urls: urls(target.turnUrls),
        username: target.turnUsername,
        credential: target.turnCredential,
      });
    }

    const action = ++connectionActionRef.current;
    const previous = clientRef.current;
    clientRef.current = null;
    abortRef.current?.abort();

    setRoutesInfo((prev) => ({ ...prev, [target.id]: '' }));
    setConnectionStates((prev) => ({ ...prev, [target.id]: 'connecting' }));

    if (previous) closingRef.current = previous.disconnect();
    await closingRef.current;

    if (!mountedRef.current || action !== connectionActionRef.current) return;

    const client = new OllamaRemoteClient({
      signalingUrl: (target.signalingUrl || getDefaultSignalingUrl()).trim(),
      room,
      token,
      iceServers,
      forceRelay: target.forceRelay,
      onState: (next, detail) => {
        if (!mountedRef.current || client !== clientRef.current) return;
        setConnectionStates((prev) => ({ ...prev, [target.id]: next }));
        if (detail.error) setError(detail.error);

        if (next === 'connected') {
          setError('');
          loadModels(client, target);
          client.getConnectionInfo().then((info) => {
            if (client === clientRef.current && mountedRef.current) {
              setRoutesInfo((prev) => ({
                ...prev,
                [target.id]: info?.relay ? 'TURN 中继' : 'WebRTC 直连',
              }));
            }
          }).catch(() => {});
        }
      },
    });

    clientRef.current = client;
    try {
      await client.connect();
    } catch (failure) {
      if (client === clientRef.current && mountedRef.current) {
        setError(failure.message);
        setConnectionStates((prev) => ({ ...prev, [target.id]: 'error' }));
      }
    }
  };

  const loadModels = async (client, agent) => {
    if (!client || !agent) return;
    const loadId = ++modelLoadRef.current;
    setLoadingModels(true);
    const apiMode = agent.apiMode || 'ollama';

    try {
      const response = await client.fetch(apiMode === 'openai' ? '/v1/models' : '/api/tags');
      if (!response.ok) throw new Error('无法读取模型列表：HTTP ' + response.status);
      const data = await response.json();
      if (client !== clientRef.current || !mountedRef.current || loadId !== modelLoadRef.current) return;

      const names = (apiMode === 'openai' ? data.data || [] : data.models || [])
        .map((m) => (apiMode === 'openai' ? m.id : m.name || m.model))
        .filter(Boolean);

      setModelsMap((prev) => ({ ...prev, [agent.id]: names }));

      if (names.length > 0 && !names.includes(agent.model)) {
        updateAgentField(agent.id, 'model', names[0]);
      }
    } catch (err) {
      if (client === clientRef.current && mountedRef.current && loadId === modelLoadRef.current) {
        console.warn('Load models failed', err);
      }
    } finally {
      if (client === clientRef.current && mountedRef.current && loadId === modelLoadRef.current) {
        setLoadingModels(false);
      }
    }
  };

  const handleDisconnect = () => {
    ++connectionActionRef.current;
    ++modelLoadRef.current;
    abortRef.current?.abort();
    const client = clientRef.current;
    clientRef.current = null;
    if (client) closingRef.current = client.disconnect();
    if (activeAgent) {
      setConnectionStates((prev) => ({ ...prev, [activeAgent.id]: 'disconnected' }));
      setRoutesInfo((prev) => ({ ...prev, [activeAgent.id]: '' }));
    }
    setError('');
    setLoadingModels(false);
    setCustomModelMode(false);
  };

  // Switch agent
  const selectAgent = (agent, autoStart = true) => {
    if (agent.id === activeAgentId) return;
    handleDisconnect();
    setActiveAgentId(agent.id);
    saveActiveAgentId(agent.id);
    window.location.hash = `#/agent/${encodeURIComponent(agent.room)}`;
    if (autoStart) {
      setTimeout(() => {
        doConnect(agent);
      }, 50);
    }
  };

  // Update field of an agent
  const updateAgentField = (agentId, key, value) => {
    setAgents((prev) => {
      const next = prev.map((a) => (a.id === agentId ? { ...a, [key]: value } : a));
      saveAgents(next);
      return next;
    });
  };

  // Add agent
  const handleAddNewAgent = () => {
    const count = agents.length + 1;
    const room = `agent-${count}`;
    const newAgent = createDefaultAgent({
      name: `Agent ${count}`,
      room,
      token: generateSecureToken(32),
    });
    const next = [...agents, newAgent];
    setAgents(next);
    saveAgents(next);
    selectAgent(newAgent, false);
    setEditingAgent(newAgent);
    setShowEditModal(true);
  };

  // Clone agent
  const handleCloneAgent = (agent) => {
    const newRoom = `${agent.room}-copy`;
    const cloned = {
      ...agent,
      id: safeRandomUUID(),
      name: `${agent.name} (副本)`,
      room: newRoom,
      token: generateSecureToken(32),
      createdAt: Date.now(),
    };
    const next = [...agents, cloned];
    setAgents(next);
    saveAgents(next);
    selectAgent(cloned, false);
  };

  // Delete agent
  const handleDeleteAgent = (agentId) => {
    if (agents.length <= 1) {
      alert('至少需要保留一个 Agent 配置');
      return;
    }
    if (!confirm('确定要删除该 Agent 吗？其独立访问路径与配置将移除。')) return;

    if (agentId === activeAgentId) {
      handleDisconnect();
    }
    const next = agents.filter((a) => a.id !== agentId);
    setAgents(next);
    saveAgents(next);

    if (agentId === activeAgentId) {
      const fallback = next[0];
      setActiveAgentId(fallback.id);
      saveActiveAgentId(fallback.id);
      window.location.hash = `#/agent/${encodeURIComponent(fallback.room)}`;
    }
  };

  // Save edited agent
  const handleSaveEdit = (e) => {
    e.preventDefault();
    if (!editingAgent) return;
    const room = editingAgent.room.trim();
    if (!/^[a-zA-Z0-9_-]{1,64}$/.test(room)) {
      alert('房间号仅支持 1-64 位字母、数字、短横线或下划线');
      return;
    }
    if (editingAgent.token.trim().length < 16) {
      alert('Token 长度至少需要 16 个字符');
      return;
    }

    setAgents((prev) => {
      const next = prev.map((a) => (a.id === editingAgent.id ? editingAgent : a));
      saveAgents(next);
      return next;
    });

    if (editingAgent.id === activeAgentId) {
      window.location.hash = `#/agent/${encodeURIComponent(room)}`;
      handleDisconnect();
      setTimeout(() => doConnect(editingAgent), 50);
    }
    setShowEditModal(false);
  };

  // Handle URL route changes
  useEffect(() => {
    mountedRef.current = true;

    const handleRoute = () => {
      const { routeRoom, routeToken, autoConnect } = parseCurrentRoute();
      if (!routeRoom) return;

      setAgents((currentAgents) => {
        let matched = currentAgents.find((a) => a.room === routeRoom);
        let nextList = currentAgents;

        if (!matched) {
          matched = createDefaultAgent({
            name: `Agent (${routeRoom})`,
            room: routeRoom,
            token: routeToken || generateSecureToken(32),
          });
          nextList = [...currentAgents, matched];
          saveAgents(nextList);
        } else if (routeToken && matched.token !== routeToken) {
          matched = { ...matched, token: routeToken };
          nextList = currentAgents.map((a) => (a.id === matched.id ? matched : a));
          saveAgents(nextList);
        }

        setActiveAgentId(matched.id);
        saveActiveAgentId(matched.id);

        if (autoConnect) {
          setTimeout(() => {
            if (mountedRef.current) doConnect(matched);
          }, 100);
        }
        return nextList;
      });
    };

    handleRoute();
    window.addEventListener('hashchange', handleRoute);

    const cleanup = () => {
      ++connectionActionRef.current;
      abortRef.current?.abort();
      const client = clientRef.current;
      clientRef.current = null;
      if (client) closingRef.current = client.disconnect();
    };
    window.addEventListener('pagehide', cleanup);

    return () => {
      mountedRef.current = false;
      cleanup();
      window.removeEventListener('hashchange', handleRoute);
      window.removeEventListener('pagehide', cleanup);
    };
  }, []);

  // Scroll to bottom on message
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'smooth', block: 'end' });
  }, [currentMessages]);

  // Copy helper
  const copyText = (key, text) => {
    navigator.clipboard?.writeText(text).then(() => {
      setCopiedKey(key);
      setTimeout(() => setCopiedKey(''), 2000);
    }).catch(() => {
      prompt('请手动复制内容：', text);
    });
  };

  // Send message
  const handleSend = async () => {
    const client = clientRef.current;
    if (!isConnected || !client || sendingRef.current || !input.trim() || !activeAgent) return;

    sendingRef.current = true;
    setBusy(true);
    setError('');

    const controller = new AbortController();
    abortRef.current = controller;

    const userMessage = { id: safeRandomUUID(), role: 'user', content: input.trim() };
    const assistantId = safeRandomUUID();
    const assistantPlaceholder = {
      id: assistantId,
      role: 'assistant',
      model: activeAgent.model,
      content: '',
    };

    const updatedMessages = [...currentMessages.filter((m) => !m.error), userMessage];
    const newAgentMessages = [...updatedMessages, assistantPlaceholder];

    setChatHistory((prev) => {
      const next = { ...prev, [activeAgent.id]: newAgentMessages };
      saveChatHistory(next);
      return next;
    });
    setInput('');

    const updateAssistant = (change) => {
      if (!mountedRef.current) return;
      setChatHistory((prev) => {
        const msgs = prev[activeAgent.id] || [];
        const nextMsgs = msgs.map((m) => (m.id === assistantId ? { ...m, ...change } : m));
        const next = { ...prev, [activeAgent.id]: nextMsgs };
        saveChatHistory(next);
        return next;
      });
    };

    try {
      const historyPayload = updatedMessages.map(({ role, content }) => ({ role, content }));
      const isOllama = activeAgent.apiMode !== 'openai';
      const endpoint = isOllama ? '/api/chat' : '/v1/chat/completions';

      const response = await client.fetch(endpoint, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          model: activeAgent.model,
          messages: historyPayload,
          stream: true,
        }),
        signal: controller.signal,
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

      if (!response.body) throw new Error('模型未返回内容');

      let content = '';
      let completed = false;
      const streamReader = isOllama ? readNdjson(response.body) : readSse(response.body);

      for await (const chunk of streamReader) {
        if (chunk.error) throw new Error(chunk.error?.message || chunk.error);
        const delta = isOllama ? chunk.message?.content : chunk.choices?.[0]?.delta?.content;
        if (delta) {
          content += delta;
          updateAssistant({ content });
        }
        if (chunk.done || (!isOllama && chunk.choices?.[0]?.finish_reason)) {
          completed = true;
        }
      }

      if (!completed) throw new Error('模型流式传输在完成前中断');
    } catch (failure) {
      if (failure.name === 'AbortError') {
        updateAssistant({ note: '已停止生成' });
      } else {
        updateAssistant({ error: failure.message });
        if (mountedRef.current) setError(failure.message);
      }
    } finally {
      if (abortRef.current === controller) abortRef.current = null;
      sendingRef.current = false;
      if (mountedRef.current) setBusy(false);
    }
  };

  const serverConfig = useMemo(() => generateServerEnvConfig(agents), [agents]);
  const activeCommands = useMemo(() => (activeAgent ? generateAgentCommands(activeAgent) : null), [activeAgent]);

  return (
    <div className="layout">
      {/* Sidebar: Multi-Agent Management */}
      <aside className={`sidebar ${sidebarOpen ? 'open' : 'collapsed'}`}>
        <div className="sidebar-header">
          <div className="sidebar-title">
            <span className="brand-dot" />
            <h2>Home Agents</h2>
          </div>
          <button
            className="icon-btn"
            title={sidebarOpen ? '收起侧边栏' : '展开侧边栏'}
            onClick={() => setSidebarOpen(!sidebarOpen)}
          >
            {sidebarOpen ? '◀' : '▶'}
          </button>
        </div>

        {sidebarOpen && (
          <>
            <div className="agent-list">
              {agents.map((agent) => {
                const isSelected = agent.id === activeAgentId;
                const status = connectionStates[agent.id] || 'disconnected';
                return (
                  <div
                    key={agent.id}
                    className={`agent-card ${isSelected ? 'active' : ''}`}
                    onClick={() => selectAgent(agent, true)}
                  >
                    <div className="agent-card-header">
                      <span className={`status-pill ${status}`} title={stateLabels[status]} />
                      <span className="agent-name" title={agent.name}>{agent.name}</span>
                      <span className="agent-room-badge">#{agent.room}</span>
                    </div>

                    <div className="agent-card-meta">
                      <span className="meta-item">
                        {agent.model || '未设模型'}
                      </span>
                      <span className="meta-route">
                        {routesInfo[agent.id] || (status === 'connected' ? '已就绪' : stateLabels[status])}
                      </span>
                    </div>

                    <div className="agent-card-actions" onClick={(e) => e.stopPropagation()}>
                      <button
                        className="tiny-btn"
                        title="复制该 Agent 专属独立访问链接"
                        onClick={() => copyText(`link-${agent.id}`, getAgentAccessUrl(agent))}
                      >
                        {copiedKey === `link-${agent.id}` ? '✓ 已复制' : '🔗 专属路径'}
                      </button>
                      <button
                        className="tiny-btn"
                        title="编辑配置"
                        onClick={() => {
                          setEditingAgent({ ...agent });
                          setShowEditModal(true);
                        }}
                      >
                        ⚙️
                      </button>
                      <button
                        className="tiny-btn"
                        title="克隆配置"
                        onClick={() => handleCloneAgent(agent)}
                      >
                        📋
                      </button>
                      {agents.length > 1 && (
                        <button
                          className="tiny-btn danger"
                          title="删除 Agent"
                          onClick={() => handleDeleteAgent(agent.id)}
                        >
                          ✕
                        </button>
                      )}
                    </div>
                  </div>
                );
              })}
            </div>

            <div className="sidebar-footer">
              <button className="primary-action-btn" onClick={handleAddNewAgent}>
                + 添加新 Agent
              </button>
              <button className="secondary-action-btn" onClick={() => setShowTokensModal(true)}>
                🔑 Token 与多端部署
              </button>
            </div>
          </>
        )}
      </aside>

      {/* Main Chat Workspace */}
      <main className="main-content">
        <header className="workspace-header">
          <div className="header-left">
            {!sidebarOpen && (
              <button className="icon-btn mr" onClick={() => setSidebarOpen(true)}>
                ☰
              </button>
            )}
            <div className="header-agent-info">
              <h1>{activeAgent ? activeAgent.name : '未选择 Agent'}</h1>
              <span className="room-path-label">
                独立路径: <code>#/agent/{activeAgent?.room}</code>
              </span>
            </div>
          </div>

          <div className="header-right">
            {activeAgent && (
              <button
                className="btn-outline"
                onClick={() => copyText('active-url', getAgentAccessUrl(activeAgent))}
                title="复制该 Agent 专属直连网址"
              >
                {copiedKey === 'active-url' ? '✓ 直链已复制' : '🔗 复制专属访问链接'}
              </button>
            )}
            <div className={`status-badge ${activeState}`}>
              <span className="dot" />
              <span>{stateLabels[activeState]}</span>
              {isConnected && activeRoute && <span className="route-badge">{activeRoute}</span>}
            </div>
            {isConnected ? (
              <button className="btn-secondary" onClick={handleDisconnect}>
                断开
              </button>
            ) : (
              <button
                className="btn-primary"
                onClick={() => doConnect(activeAgent)}
                disabled={activeState === 'connecting'}
              >
                {activeState === 'connecting' ? '正在连接…' : '连接'}
              </button>
            )}
          </div>
        </header>

        {/* Chat Toolbar */}
        <div className="chat-toolbar">
          <div className="toolbar-left">
            <span className="toolbar-label">模型:</span>
            {customModelMode ? (
              <div className="custom-model-input-group">
                <input
                  type="text"
                  value={activeAgent?.model || ''}
                  onChange={(e) => updateAgentField(activeAgent.id, 'model', e.target.value)}
                  placeholder="如 llama3:8b, qwen2.5:14b"
                  disabled={busy}
                />
                <button className="text-btn" onClick={() => setCustomModelMode(false)}>
                  列表
                </button>
              </div>
            ) : (
              <select
                className="model-select"
                value={activeAgent?.model || ''}
                onChange={(e) => {
                  if (e.target.value === '__custom__') {
                    setCustomModelMode(true);
                  } else {
                    updateAgentField(activeAgent.id, 'model', e.target.value);
                  }
                }}
                disabled={busy || !isConnected || loadingModels}
              >
                {loadingModels ? (
                  <option value="">获取模型列表中...</option>
                ) : !isConnected ? (
                  <option value={activeAgent?.model}>{activeAgent?.model || '连接后自动获取'}</option>
                ) : activeModels.length === 0 ? (
                  <>
                    <option value={activeAgent?.model}>{activeAgent?.model || '未找到模型'}</option>
                    <option value="__custom__">✏️ 自定义输入模型...</option>
                  </>
                ) : (
                  <>
                    {activeModels.map((m) => (
                      <option key={m} value={m}>
                        {m}
                      </option>
                    ))}
                    {!activeModels.includes(activeAgent?.model) && activeAgent?.model && (
                      <option value={activeAgent.model}>{activeAgent.model} (自定义)</option>
                    )}
                    <option value="__custom__">✏️ 自定义输入模型...</option>
                  </>
                )}
              </select>
            )}

            {isConnected && (
              <button
                className="text-btn"
                onClick={() => clientRef.current && loadModels(clientRef.current, activeAgent)}
                disabled={loadingModels || busy}
                title="重新加载可用模型"
              >
                {loadingModels ? '刷新中…' : '🔄 刷新模型'}
              </button>
            )}
          </div>

          <div className="toolbar-right">
            <button
              className="text-btn"
              onClick={() => {
                if (!activeAgent) return;
                setChatHistory((prev) => {
                  const next = { ...prev, [activeAgent.id]: [] };
                  saveChatHistory(next);
                  return next;
                });
              }}
              disabled={busy || currentMessages.length === 0}
            >
              清空记录
            </button>
          </div>
        </div>

        {/* Chat Message List */}
        <div className="message-container">
          {currentMessages.length === 0 && (
            <div className="empty-chat-state">
              <div className="empty-icon">⚡</div>
              <h3>{activeAgent?.name || 'Home Agent'} 待命中</h3>
              <p>
                {isConnected
                  ? 'WebRTC DataChannel 桥接已建立，发送消息即可直接调用远端模型。'
                  : '点击右上角“连接”按钮，或使用专属访问路径一键直连。'}
              </p>
              {activeCommands && (
                <div className="quick-agent-tip">
                  <span>远端 Agent 启动命令：</span>
                  <code>{activeCommands.bash}</code>
                  <button
                    className="tiny-btn"
                    onClick={() => copyText('quick-cmd', activeCommands.bash)}
                  >
                    {copiedKey === 'quick-cmd' ? '已复制' : '复制命令'}
                  </button>
                </div>
              )}
            </div>
          )}

          {currentMessages.map((m) => (
            <div key={m.id} className={`message-row ${m.role}`}>
              <div className="message-bubble">
                <div className="message-header">
                  <span className="sender-tag">{m.role === 'user' ? '你' : m.model || 'Agent'}</span>
                </div>
                <div className="message-body">
                  {m.content || (busy && m.role === 'assistant' ? '正在流式响应…' : '')}
                </div>
                {m.error && <div className="message-error-badge">⚠️ {m.error}</div>}
                {m.note && <div className="message-note-badge">ℹ️ {m.note}</div>}
              </div>
            </div>
          ))}
          <div ref={bottomRef} />
        </div>

        {error && <div className="global-error-banner">{error}</div>}

        {/* Input Composer */}
        <div className="chat-composer">
          <textarea
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder={
              isConnected
                ? `发消息给 ${activeAgent?.name || 'Agent'} (Enter 发送, Shift+Enter 换行)`
                : '请先连接 Agent 建立 WebRTC 通道'
            }
            disabled={!isConnected}
            rows={3}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                handleSend();
              }
            }}
          />
          <div className="composer-actions">
            {busy ? (
              <button className="btn-secondary danger" onClick={() => abortRef.current?.abort()}>
                停止生成
              </button>
            ) : (
              <button
                className="btn-primary send-btn"
                disabled={!isConnected || !input.trim()}
                onClick={handleSend}
              >
                发送
              </button>
            )}
          </div>
        </div>
      </main>

      {/* Edit Agent Modal */}
      {showEditModal && editingAgent && (
        <div className="modal-overlay" onClick={() => setShowEditModal(false)}>
          <div className="modal-card" onClick={(e) => e.stopPropagation()}>
            <div className="modal-header">
              <h3>编辑 Agent: {editingAgent.name}</h3>
              <button className="close-btn" onClick={() => setShowEditModal(false)}>✕</button>
            </div>
            <form onSubmit={handleSaveEdit}>
              <div className="form-grid">
                <div className="form-group">
                  <label>Agent 名称</label>
                  <input
                    type="text"
                    required
                    value={editingAgent.name}
                    onChange={(e) => setEditingAgent({ ...editingAgent, name: e.target.value })}
                  />
                </div>
                <div className="form-group">
                  <label>房间号 (Room ID，作为独立访问路径)</label>
                  <input
                    type="text"
                    required
                    value={editingAgent.room}
                    onChange={(e) => setEditingAgent({ ...editingAgent, room: e.target.value })}
                  />
                  <small className="help-text">访问路径将对应为 <code>#/agent/{editingAgent.room}</code></small>
                </div>

                <div className="form-group full-width">
                  <label>专属 Token</label>
                  <div className="input-with-button">
                    <input
                      type="text"
                      required
                      value={editingAgent.token}
                      onChange={(e) => setEditingAgent({ ...editingAgent, token: e.target.value })}
                    />
                    <button
                      type="button"
                      className="btn-secondary"
                      onClick={() => setEditingAgent({ ...editingAgent, token: generateSecureToken(32) })}
                    >
                      🎲 随机生成
                    </button>
                  </div>
                </div>

                <div className="form-group full-width">
                  <label>信令服务器地址 (WebSocket)</label>
                  <input
                    type="text"
                    required
                    value={editingAgent.signalingUrl}
                    onChange={(e) => setEditingAgent({ ...editingAgent, signalingUrl: e.target.value })}
                  />
                </div>

                <div className="form-group">
                  <label>接口协议</label>
                  <select
                    value={editingAgent.apiMode || 'ollama'}
                    onChange={(e) => setEditingAgent({ ...editingAgent, apiMode: e.target.value })}
                  >
                    <option value="ollama">Ollama 原生 (/api/chat)</option>
                    <option value="openai">OpenAI 兼容 (/v1/chat/completions)</option>
                  </select>
                </div>

                <div className="form-group">
                  <label>默认模型</label>
                  <input
                    type="text"
                    value={editingAgent.model}
                    onChange={(e) => setEditingAgent({ ...editingAgent, model: e.target.value })}
                  />
                </div>

                <div className="form-group full-width">
                  <label>STUN 服务器 (逗号或空格分隔，可选)</label>
                  <input
                    type="text"
                    value={editingAgent.stunUrls || ''}
                    onChange={(e) => setEditingAgent({ ...editingAgent, stunUrls: e.target.value })}
                    placeholder="stun:stun.cloudflare.com:3478"
                  />
                </div>

                <div className="form-group full-width">
                  <label>TURN 服务器 (可选)</label>
                  <input
                    type="text"
                    value={editingAgent.turnUrls || ''}
                    onChange={(e) => setEditingAgent({ ...editingAgent, turnUrls: e.target.value })}
                    placeholder="turns:turn.example.com:443?transport=tcp"
                  />
                </div>

                <div className="form-group">
                  <label>TURN 用户名</label>
                  <input
                    type="text"
                    value={editingAgent.turnUsername || ''}
                    onChange={(e) => setEditingAgent({ ...editingAgent, turnUsername: e.target.value })}
                  />
                </div>
                <div className="form-group">
                  <label>TURN 密码</label>
                  <input
                    type="password"
                    value={editingAgent.turnCredential || ''}
                    onChange={(e) => setEditingAgent({ ...editingAgent, turnCredential: e.target.value })}
                  />
                </div>

                <div className="form-group full-width checkbox-row">
                  <label>
                    <input
                      type="checkbox"
                      checked={editingAgent.forceRelay || false}
                      onChange={(e) => setEditingAgent({ ...editingAgent, forceRelay: e.target.checked })}
                    />
                    仅使用 TURN 中继模式 (适用于对称 NAT 或纯中继连通测试)
                  </label>
                </div>
              </div>

              <div className="modal-footer">
                <button type="button" className="btn-secondary" onClick={() => setShowEditModal(false)}>
                  取消
                </button>
                <button type="submit" className="btn-primary">
                  保存并应用
                </button>
              </div>
            </form>
          </div>
        </div>
      )}

      {/* Multi-Token & Deployment Config Modal */}
      {showTokensModal && (
        <div className="modal-overlay" onClick={() => setShowTokensModal(false)}>
          <div className="modal-card wide-modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-header">
              <h3>🔑 多 Token 管理与信令服务端配置</h3>
              <button className="close-btn" onClick={() => setShowTokensModal(false)}>✕</button>
            </div>

            <div className="tokens-modal-content">
              <section className="config-section">
                <h4>1. 信令服务器环境变量配置 (ROOM_TOKENS_JSON)</h4>
                <p>
                  <code>ai-remote-signaling</code> 支持通过环境变量 <code>ROOM_TOKENS_JSON</code> 为各个房间指定不同的 Token。
                  下方已根据你配置的 {agents.length} 个 Agent 自动汇总：
                </p>
                <div className="code-box">
                  <pre>{serverConfig.envExport}</pre>
                  <button
                    className="copy-corner-btn"
                    onClick={() => copyText('env-export', serverConfig.envExport)}
                  >
                    {copiedKey === 'env-export' ? '✓ 已复制' : '复制配置'}
                  </button>
                </div>
              </section>

              <section className="config-section">
                <h4>2. 各 Home-Agent 启动命令汇总与独立路径</h4>
                <div className="agent-deployment-table-wrapper">
                  <table className="deployment-table">
                    <thead>
                      <tr>
                        <th>Agent 名称</th>
                        <th>Room ID</th>
                        <th>Token</th>
                        <th>独立访问路径</th>
                        <th>启动命令</th>
                      </tr>
                    </thead>
                    <tbody>
                      {agents.map((ag) => {
                        const cmds = generateAgentCommands(ag);
                        const url = getAgentAccessUrl(ag);
                        return (
                          <tr key={ag.id}>
                            <td><strong>{ag.name}</strong></td>
                            <td><code>{ag.room}</code></td>
                            <td>
                              <span className="token-masked" title={ag.token}>
                                {ag.token.slice(0, 6)}...{ag.token.slice(-4)}
                              </span>
                            </td>
                            <td>
                              <button
                                className="tiny-btn"
                                onClick={() => copyText(`link-tb-${ag.id}`, url)}
                              >
                                {copiedKey === `link-tb-${ag.id}` ? '✓ 已复制' : '复制访问直链'}
                              </button>
                            </td>
                            <td>
                              <button
                                className="tiny-btn"
                                onClick={() => copyText(`cmd-tb-${ag.id}`, cmds.bash)}
                              >
                                {copiedKey === `cmd-tb-${ag.id}` ? '✓ 已复制' : '复制 Bash 命令'}
                              </button>
                              <button
                                className="tiny-btn"
                                onClick={() => copyText(`cmd-ps-${ag.id}`, cmds.powershell)}
                              >
                                {copiedKey === `cmd-ps-${ag.id}` ? '✓ 已复制' : '复制 PowerShell'}
                              </button>
                            </td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                </div>
              </section>
            </div>

            <div className="modal-footer">
              <button className="btn-primary" onClick={() => setShowTokensModal(false)}>
                完成
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
