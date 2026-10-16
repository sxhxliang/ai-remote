import React, { useState } from 'react';
import { safeRandomUUID } from './webrtcClient.js';

const ASPECT_RATIOS = ['1:1', '16:9', '9:16', '4:3', '3:4', '3:2', '2:3', '21:9', '9:21'];
const COUNTS = [1, 2, 3, 4];

// MiniMax image-01 text-to-image, tunneled through the home agent.
// The API key is injected server-side by the agent and never reaches the browser;
// images come back as CDN URLs (response_format: 'url') and load directly.
export default function ImageGen({ getClient, connected }) {
  const [prompt, setPrompt] = useState('');
  const [aspectRatio, setAspectRatio] = useState('1:1');
  const [count, setCount] = useState(1);
  const [promptOptimizer, setPromptOptimizer] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [items, setItems] = useState([]);

  const generate = async () => {
    const client = getClient();
    if (!connected || !client || busy || !prompt.trim()) return;
    setBusy(true);
    setError('');
    try {
      const response = await client.minimaxFetch('/v1/image_generation', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          model: 'image-01',
          prompt: prompt.trim(),
          aspect_ratio: aspectRatio,
          n: count,
          response_format: 'url',
          prompt_optimizer: promptOptimizer,
        }),
      });
      const data = await response.json().catch(() => ({}));
      if (!response.ok) throw new Error(data.error || 'HTTP ' + response.status);
      if (data.base_resp && data.base_resp.status_code !== 0) {
        throw new Error(data.base_resp.status_msg || 'MiniMax 返回错误');
      }
      const urls = data.data?.image_urls || [];
      if (!urls.length) throw new Error('MiniMax 没有返回图片');
      setItems((previous) => [{
        id: safeRandomUUID(),
        prompt: prompt.trim(),
        aspectRatio,
        urls,
        time: new Date().toLocaleString(),
      }, ...previous]);
    } catch (failure) {
      setError(failure.name === 'AbortError' ? '请求已取消' : failure.message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="imagegen" aria-label="生图">
      <div className="imagegen-form">
        <label className="wide">提示词
          <textarea
            value={prompt}
            onChange={(event) => setPrompt(event.target.value)}
            rows={3}
            placeholder={connected ? '描述你想要的画面，Enter 生成，Shift + Enter 换行' : '连接家里的 Agent 后开始生图'}
            disabled={!connected || busy}
            onKeyDown={(event) => {
              if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
                event.preventDefault();
                generate();
              }
            }}
          />
        </label>
        <label>画面比例
          <select value={aspectRatio} onChange={(event) => setAspectRatio(event.target.value)} disabled={busy}>
            {ASPECT_RATIOS.map((ratio) => <option key={ratio} value={ratio}>{ratio}</option>)}
          </select>
        </label>
        <label>生成张数
          <select value={count} onChange={(event) => setCount(Number(event.target.value))} disabled={busy}>
            {COUNTS.map((n) => <option key={n} value={n}>{n} 张</option>)}
          </select>
        </label>
        <label className="check wide"><input type="checkbox" checked={promptOptimizer} onChange={(event) => setPromptOptimizer(event.target.checked)} disabled={busy} />提示词优化（让模型自动改写提示词）</label>
        <div className="imagegen-actions">
          <button onClick={generate} disabled={!connected || busy || !prompt.trim()}>{busy ? '生成中…' : '生成图片'}</button>
          <span className="hint">MiniMax image-01 · 经家里 Agent 中转，Key 不出内网</span>
        </div>
      </div>
      {error && <div className="error" role="alert">{error}</div>}
      <div className="imagegen-results">
        {!items.length && (
          <div className="empty-state">
            <span>还没有作品。</span>
            <p>{connected ? '在上方输入提示词，生成第一张图。' : '连接家里的 Agent 后开始生图。'}</p>
          </div>
        )}
        {items.map((item) => (
          <article key={item.id} className="imagegen-item">
            <div className="imagegen-meta"><span>{item.time}</span><span>{item.aspectRatio}</span></div>
            <p className="imagegen-prompt">{item.prompt}</p>
            <div className="imagegen-grid">
              {item.urls.map((url, index) => (
                <a key={index} href={url} target="_blank" rel="noreferrer" title="在新标签页打开原图">
                  <img src={url} alt={item.prompt} loading="lazy" />
                </a>
              ))}
            </div>
          </article>
        ))}
      </div>
    </section>
  );
}
