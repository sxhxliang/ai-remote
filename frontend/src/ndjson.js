export async function* readNdjson(stream) {
  const reader = stream.getReader();
  const decoder = new TextDecoder('utf-8', { fatal: true });
  let buffer = '';
  let finished = false;
  try {
    while (true) {
      const { value, done } = await reader.read();
      buffer += done ? decoder.decode() : decoder.decode(value, { stream: true });
      let newline;
      while ((newline = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, newline).trim();
        buffer = buffer.slice(newline + 1);
        if (line) yield JSON.parse(line);
      }
      if (done) {
        finished = true;
        if (buffer.trim()) yield JSON.parse(buffer);
        break;
      }
      if (buffer.length > 8 * 1024 * 1024) throw new Error('模型返回的数据行过长');
    }
  } finally {
    if (!finished) await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}
