export async function* readSse(stream) {
  const reader = stream.getReader();
  const decoder = new TextDecoder('utf-8', { fatal: true });
  let buffer = '';
  let data = [];
  let finished = false;
  const event = () => {
    const value = data.join('\n');
    data = [];
    return value;
  };
  try {
    while (true) {
      const next = await reader.read();
      buffer += next.done ? decoder.decode() : decoder.decode(next.value, { stream: true });
      let newline;
      while ((newline = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, newline).replace(/\r$/, '');
        buffer = buffer.slice(newline + 1);
        if (line.startsWith('data:')) data.push(line.slice(5).trimStart());
        else if (line === '') {
          const value = event();
          if (value === '[DONE]') { finished = true; yield { done: true }; return; }
          if (value) yield JSON.parse(value);
        }
      }
      if (next.done) {
        if (buffer.startsWith('data:')) data.push(buffer.slice(5).trimStart());
        const value = event();
        if (value === '[DONE]') yield { done: true };
        else if (value) yield JSON.parse(value);
        finished = true;
        return;
      }
      if (buffer.length > 8 * 1024 * 1024) throw new Error('模型返回的数据行过长');
    }
  } finally {
    if (!finished) await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}
