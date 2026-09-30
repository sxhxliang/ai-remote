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

export function generateSecureToken(length = 32) {
  const charset = 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789';
  if (typeof crypto !== 'undefined' && typeof crypto.getRandomValues === 'function') {
    const bytes = new Uint8Array(length);
    crypto.getRandomValues(bytes);
    return Array.from(bytes, (b) => charset[b % charset.length]).join('');
  }
  let res = '';
  for (let i = 0; i < length; i++) {
    res += charset.charAt(Math.floor(Math.random() * charset.length));
  }
  return res;
}

const isLocal = ['localhost', '127.0.0.1', '[::1]'].includes(typeof location !== 'undefined' ? location.hostname : '');

export function getDefaultSignalingUrl() {
  if (typeof location === 'undefined') return 'ws://127.0.0.1:8080/ws';
  return isLocal
    ? 'ws://127.0.0.1:8080/ws'
    : (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/ws';
}

export function createDefaultAgent(overrides = {}) {
  const room = overrides.room || 'default';
  const name = overrides.name || (room === 'default' ? '默认 Agent' : `Agent-${room}`);
  return {
    id: overrides.id || safeRandomUUID(),
    name,
    room,
    token: overrides.token || generateSecureToken(32),
    signalingUrl: overrides.signalingUrl || getDefaultSignalingUrl(),
    model: overrides.model || 'qwen2.5:7b',
    apiMode: overrides.apiMode || 'ollama',
    stunUrls: overrides.stunUrls || '',
    turnUrls: overrides.turnUrls || '',
    turnUsername: overrides.turnUsername || '',
    turnCredential: overrides.turnCredential || '',
    forceRelay: overrides.forceRelay ?? false,
    description: overrides.description || '',
    createdAt: overrides.createdAt || Date.now(),
  };
}

const AGENTS_STORAGE_KEY = 'ai_remote_agents_v1';
const ACTIVE_AGENT_KEY = 'ai_remote_active_agent_id';
const CHAT_HISTORY_KEY = 'ai_remote_chat_history_v1';

export function loadAgents() {
  try {
    const raw = localStorage.getItem(AGENTS_STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed) && parsed.length > 0) {
        return parsed;
      }
    }
  } catch (e) {
    console.warn('Failed to load agents from localStorage', e);
  }
  const defaultAgent = createDefaultAgent({ name: '家庭默认 Agent', room: 'default' });
  saveAgents([defaultAgent]);
  return [defaultAgent];
}

export function saveAgents(agents) {
  try {
    localStorage.setItem(AGENTS_STORAGE_KEY, JSON.stringify(agents));
  } catch (e) {
    console.error('Failed to save agents to localStorage', e);
  }
}

export function loadActiveAgentId() {
  try {
    return localStorage.getItem(ACTIVE_AGENT_KEY) || null;
  } catch (e) {
    return null;
  }
}

export function saveActiveAgentId(id) {
  try {
    if (id) localStorage.setItem(ACTIVE_AGENT_KEY, id);
    else localStorage.removeItem(ACTIVE_AGENT_KEY);
  } catch (e) {}
}

export function loadChatHistory() {
  try {
    const raw = localStorage.getItem(CHAT_HISTORY_KEY);
    return raw ? JSON.parse(raw) : {};
  } catch (e) {
    return {};
  }
}

export function saveChatHistory(history) {
  try {
    localStorage.setItem(CHAT_HISTORY_KEY, JSON.stringify(history));
  } catch (e) {}
}

export function generateServerEnvConfig(agents) {
  const roomTokens = {};
  for (const agent of agents) {
    if (agent.room && agent.token) {
      roomTokens[agent.room.trim()] = agent.token.trim();
    }
  }
  const jsonStr = JSON.stringify(roomTokens);
  return {
    roomTokens,
    jsonString: jsonStr,
    envExport: `ROOM_TOKENS_JSON='${jsonStr}'`,
  };
}

export function generateAgentCommands(agent, signalingWsUrl) {
  const ws = signalingWsUrl || agent.signalingUrl || getDefaultSignalingUrl();
  const room = agent.room || 'default';
  const token = agent.token || '';
  
  return {
    bash: `SIGNALING_URL="${ws}" ROOM_ID="${room}" SIGNALING_TOKEN="${token}" ai-remote-agent`,
    powershell: `$env:SIGNALING_URL="${ws}"; $env:ROOM_ID="${room}"; $env:SIGNALING_TOKEN="${token}"; ai-remote-agent`,
    docker: `docker run -d --name ai-agent-${room} --network host \\\n  -e SIGNALING_URL="${ws}" \\\n  -e ROOM_ID="${room}" \\\n  -e SIGNALING_TOKEN="${token}" \\\n  sxhxliang/ai-remote-agent:latest`,
  };
}

export function getAgentAccessUrl(agent, origin = '') {
  const base = origin || (typeof window !== 'undefined' ? window.location.origin + window.location.pathname : '');
  return `${base}#/agent/${encodeURIComponent(agent.room)}?token=${encodeURIComponent(agent.token)}`;
}
