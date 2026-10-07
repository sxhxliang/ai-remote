import React, { useEffect, useRef, useState } from 'react';
import Markdown from 'react-markdown';
import remarkGfm from 'remark-gfm';

async function fileRequest(client, endpoint, signal, body) {
  const response = await client.fetch('/files/' + endpoint, {
    signal,
    ...(body ? { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) } : {}),
  });
  const result = await response.json();
  if (!response.ok) throw new Error(result.error || '文件请求失败：HTTP ' + response.status);
  return result;
}

// Do not load images from a document into the browser. Relative links to other
// Markdown files are opened through the same confined file API.
function previewLink(href, currentPath) {
  if (!href || /^(?:[a-z][a-z\d+.-]*:|\/\/)/i.test(href)) return null;
  try {
    const path = new URL(href, 'https://files.invalid/' + currentPath).pathname.slice(1);
    const decoded = decodeURIComponent(path);
    return /\.(md|markdown)$/i.test(decoded) ? decoded : null;
  } catch { return null; }
}

export default function FileBrowser({ client, connected, onDirtyChange }) {
  const [capabilities, setCapabilities] = useState(null);
  const [directory, setDirectory] = useState('');
  const directoryRef = useRef('');
  const [entries, setEntries] = useState([]);
  const [document, setDocument] = useState(null);
  const [draft, setDraft] = useState('');
  const [editing, setEditing] = useState(false);
  const [newName, setNewName] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const operationRef = useRef(null);
  const dirty = !!document && (document.revision === null || draft !== document.content);
  const byteLength = new TextEncoder().encode(draft).length;
  const tooLarge = byteLength > (capabilities?.maxFileBytes || 512 * 1024);

  useEffect(() => { onDirtyChange(dirty); }, [dirty, onDirtyChange]);
  useEffect(() => {
    if (!dirty) return;
    const warn = (event) => { event.preventDefault(); event.returnValue = ''; };
    window.addEventListener('beforeunload', warn);
    return () => window.removeEventListener('beforeunload', warn);
  }, [dirty]);

  const operate = async (work) => {
    if (!client || !connected || operationRef.current) return;
    const controller = new AbortController();
    operationRef.current = controller;
    setBusy(true);
    setError('');
    setNotice('');
    const request = async (endpoint, body) => {
      const result = await fileRequest(client, endpoint, controller.signal, body);
      if (controller.signal.aborted) throw new DOMException('Cancelled', 'AbortError');
      return result;
    };
    try { await work(request); } catch (failure) {
      if (!controller.signal.aborted) setError(failure.message);
    } finally {
      if (operationRef.current === controller) {
        operationRef.current = null;
        setBusy(false);
      }
    }
  };

  const readDirectory = async (request, path) => {
    const data = await request('list?path=' + encodeURIComponent(path));
    setDirectory(data.path);
    directoryRef.current = data.path;
    setEntries(data.entries);
  };

  useEffect(() => {
    setDocument(null);
    setDraft('');
    setCapabilities(null);
    setEntries([]);
    setDirectory('');
    directoryRef.current = '';
    setNewName('');
    setError('');
    setNotice('');
  }, [client]);

  useEffect(() => {
    if (connected && client) {
      operate(async (request) => {
        const settings = await request('config');
        setCapabilities(settings);
        if (settings.enabled) await readDirectory(request, directoryRef.current);
      });
    }
    return () => {
      operationRef.current?.abort();
      operationRef.current = null;
      setBusy(false);
    };
  }, [client, connected]);

  const canReplaceDraft = () => !dirty || window.confirm('当前文件有未保存的修改，是否放弃这些修改？');
  const openDocument = (path) => {
    if (!canReplaceDraft()) return;
    operate(async (request) => {
      const data = await request('read?path=' + encodeURIComponent(path));
      setDocument(data);
      setDraft(data.content);
      setEditing(false);
    });
  };
  const createDocument = (event) => {
    event.preventDefault();
    if (!canReplaceDraft()) return;
    const name = newName.trim();
    if (!name || /[\\/]/.test(name) || !/\.(md|markdown)$/i.test(name)) {
      setError('请输入当前目录中的 Markdown 文件名，例如 notes.md');
      return;
    }
    setDocument({ path: [directory, name].filter(Boolean).join('/'), content: '', revision: null });
    setDraft('');
    setEditing(true);
    setNewName('');
    setError('');
    setNotice('新文件将在保存后创建。');
  };
  const save = () => operate(async (request) => {
    const saved = await request('write', { path: document.path, content: draft, revision: document.revision });
    setDocument({ ...saved, content: draft });
    setNotice('已保存到家里的电脑。');
    await readDirectory(request, directoryRef.current);
  });
  const unavailable = !connected || busy;

  return <section className="file-browser" aria-label="远程文件">
    <div className="file-toolbar">
      <div><strong>Markdown 文件</strong><span className="file-mode">{capabilities?.enabled ? (capabilities.writable ? '可读写' : '只读') : '本地目录共享'}</span></div>
      <button className="secondary" disabled={unavailable} onClick={() => operate(async (request) => {
        const settings = await request('config');
        setCapabilities(settings);
        if (settings.enabled) await readDirectory(request, directory);
      })}>{busy ? '处理中…' : '刷新目录'}</button>
    </div>
    {!connected && <p className="file-hint">连接家里的 Agent 后浏览文件。连接中断时，当前草稿会保留在页面中。</p>}
    {connected && capabilities && !capabilities.enabled && <div className="file-hint">
      <p>Agent 尚未开启文件共享。</p>
      <p>在家庭电脑的配置中设置 <code>FILE_ROOT=/你的/Markdown目录</code> 后重启 Agent。需要编辑时，再设置 <code>FILE_WRITE_ENABLED=true</code>。</p>
    </div>}
    {error && <div className="error" role="alert">{error}{document && <span> 草稿仍保留在编辑器中；保存结果不确定时，请先重新读取文件核对。</span>}</div>}
    {notice && <p className="file-notice" role="status">{notice}</p>}
    {(capabilities?.enabled || document) && <div className="file-layout">
      <aside className="file-sidebar" aria-label="目录">
        <nav className="file-breadcrumbs" aria-label="目录路径">
          <button className="text-button" disabled={unavailable} onClick={() => operate((request) => readDirectory(request, ''))}>共享目录</button>
          {directory.split('/').filter(Boolean).map((name, index, parts) => <React.Fragment key={index}>
            <span>/</span><button className="text-button" disabled={unavailable} onClick={() => operate((request) => readDirectory(request, parts.slice(0, index + 1).join('/')))}>{name}</button>
          </React.Fragment>)}
        </nav>
        {directory && <button className="file-entry" disabled={unavailable} onClick={() => operate((request) => readDirectory(request, directory.split('/').slice(0, -1).join('/')))}>↑ 上级目录</button>}
        <div className="file-entries">
          {entries.map((entry) => <button key={entry.path} className={'file-entry ' + (entry.path === document?.path ? 'selected' : '')} disabled={unavailable} onClick={() => entry.kind === 'directory' ? operate((request) => readDirectory(request, entry.path)) : openDocument(entry.path)}>
            <span aria-hidden="true">{entry.kind === 'directory' ? '▸' : '≡'}</span><span>{entry.name}</span>
          </button>)}
          {!entries.length && !busy && <p className="file-hint">此目录没有可显示的子目录或 Markdown 文件。</p>}
        </div>
        {capabilities?.writable && <form className="new-file" onSubmit={createDocument}>
          <label>新建文件<input value={newName} onChange={(event) => setNewName(event.target.value)} placeholder="notes.md" disabled={unavailable} /></label>
          <button className="secondary" disabled={unavailable || !newName.trim()}>新建</button>
        </form>}
      </aside>
      <div className="file-document">
        {!document ? <div className="empty-state"><span>选择一篇 Markdown</span><p>浏览家里的笔记与文档。</p></div> : <>
          <div className="document-heading"><strong>{document.path}</strong><span>{dirty ? '未保存' : '已同步'}</span></div>
          <div className="document-actions">
            <button className={editing ? 'secondary' : ''} onClick={() => setEditing(false)}>预览</button>
            <button className={editing ? '' : 'secondary'} onClick={() => setEditing(true)}>{capabilities?.writable ? '编辑' : '源码'}</button>
            <button className="text-button" disabled={unavailable} onClick={() => openDocument(document.path)}>重新读取</button>
            {capabilities?.writable && <button disabled={unavailable || !dirty || tooLarge} onClick={save}>保存</button>}
          </div>
          {tooLarge && <p className="error" role="alert">文件内容超过 512 KiB，请缩短后再保存。</p>}
          {editing ? <textarea className="markdown-editor" aria-label="Markdown 内容" spellCheck={false} value={draft} onChange={(event) => { setDraft(event.target.value); setNotice(''); }} readOnly={!capabilities?.writable || busy} /> :
            <article className="markdown-preview"><Markdown remarkPlugins={[remarkGfm]} skipHtml components={{
              img: ({ alt }) => <span className="image-placeholder">[图片：{alt || '未加载'}]</span>,
              a: ({ href, children }) => {
                const path = previewLink(href, document.path);
                if (path) return <button className="markdown-link" disabled={unavailable} onClick={() => openDocument(path)}>{children}</button>;
                return /^(https?:|mailto:)/i.test(href || '') ? <a href={href} target="_blank" rel="noreferrer noopener">{children}</a> : <span>{children}</span>;
              },
            }}>{draft}</Markdown>{!draft && <p className="file-hint">空文档</p>}</article>}
          <div className="document-footer">UTF-8 · {(byteLength / 1024).toFixed(1)} KiB / 512 KiB</div>
        </>}
      </div>
    </div>}
  </section>;
}
