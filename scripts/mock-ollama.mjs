import http from 'node:http';
import { pathToFileURL } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const models = ['qwen2.5:7b', 'mock:latest'];

function json(response, status, body) {
  response.writeHead(status, { 'Content-Type': 'application/json' });
  response.end(JSON.stringify(body));
}

async function readJson(request) {
  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    if (size > 1024 * 1024) throw new Error('Request body exceeds 1 MiB');
    chunks.push(chunk);
  }
  return JSON.parse(Buffer.concat(chunks).toString('utf8') || '{}');
}

export function createMockOllamaServer() {
  const requests = [];
  return http.createServer(async (request, response) => {
    const path = new URL(request.url, 'http://127.0.0.1').pathname;
    if (path === '/__mock__/requests' && request.method === 'GET') {
      json(response, 200, requests);
      return;
    }
    const trace = { method: request.method, path, scenario: request.headers['x-mock-scenario'] || null, aborted: false };
    requests.push(trace);
    response.once('close', () => { if (!response.writableFinished) trace.aborted = true; });
    if (requests.length > 1000) requests.shift();

    if (path === '/' && request.method === 'GET') {
      response.end('Mock Ollama is running. No real model is used.');
      return;
    }
    if (path === '/api/version' && request.method === 'GET') {
      json(response, 200, { version: '0.0.0-mock' });
      return;
    }
    if (path === '/api/tags' && request.method === 'GET') {
      json(response, 200, {
        models: models.map((name) => ({
          name,
          model: name,
          modified_at: '2026-09-08T00:00:00Z',
          size: 0,
          digest: 'mock-no-model-weights',
          details: { family: 'mock', families: ['mock'], parameter_size: '0', quantization_level: 'none' },
        })),
      });
      return;
    }
    if (path === '/api/delete' && request.method === 'DELETE') {
      json(response, 200, { mock: true, deleted: false });
      return;
    }
    if (!['/api/chat', '/api/generate'].includes(path) || request.method !== 'POST') {
      json(response, 404, { error: 'Mock Ollama: endpoint not found' });
      return;
    }

    try {
      const body = await readJson(request);
      if (!models.includes(body.model)) {
        json(response, 404, { error: `model '${body.model}' not found` });
        return;
      }
      const scenario = request.headers['x-mock-scenario'];
      if (scenario === 'http-error') {
        json(response, 503, { error: 'Mock Ollama: simulated unavailable' });
        return;
      }
      if (scenario === 'stall') {
        await new Promise((done) => response.once('close', done));
        return;
      }
      const parts = ['这是本地 Mock Ollama。', '中文流式响应正常，', '不会调用真实模型。'];
      const record = (content, done = false) => ({
        model: body.model,
        created_at: new Date().toISOString(),
        ...(path === '/api/chat' ? { message: { role: 'assistant', content } } : { response: content }),
        done,
        ...(done ? { done_reason: 'stop', total_duration: 1000000, eval_count: 3 } : {}),
      });
      if (body.stream === false) {
        json(response, 200, record(parts.join(''), true));
        return;
      }
      response.writeHead(200, { 'Content-Type': 'application/x-ndjson' });
      if (scenario === 'stall-stream') {
        response.write(JSON.stringify(record('等待超时测试')) + '\n');
        await new Promise((done) => response.once('close', done));
        return;
      }
      if (scenario === 'burst') {
        for (let index = 0; index < 512; index++) {
          if (response.destroyed) return;
          response.write(JSON.stringify(record('x'.repeat(2048))) + '\n');
          await delay(2);
        }
        if (!response.destroyed) response.end(JSON.stringify(record('', true)) + '\n');
        return;
      }
      if (scenario === 'utf8-split') {
        const bytes = Buffer.from(JSON.stringify(record('你好，世界🌍')) + '\n');
        const cut = bytes.indexOf(Buffer.from('你')) + 1;
        response.write(bytes.subarray(0, cut));
        await delay(100);
        if (response.destroyed) return;
        response.write(bytes.subarray(cut));
      } else {
        for (const part of parts) {
          if (response.destroyed) return;
          response.write(JSON.stringify(record(part)) + '\n');
          await delay(150);
          if (scenario === 'disconnect') {
            response.destroy();
            return;
          }
        }
      }
      if (!response.destroyed) response.end(JSON.stringify(record('', true)) + '\n');
    } catch (error) {
      if (!response.headersSent && !response.destroyed) {
        json(response, 400, { error: error.message });
      } else {
        response.destroy();
      }
    }
  });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const port = Number(process.env.MOCK_OLLAMA_PORT || 11434);
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('Invalid MOCK_OLLAMA_PORT');
  const server = createMockOllamaServer();
  server.on('error', (error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
  server.listen(port, '127.0.0.1', () => {
    console.log(`Mock Ollama: http://127.0.0.1:${port} (no real model)`);
    console.log(`Models: ${models.join(', ')}`);
  });
  const stop = () => {
    server.close();
    server.closeAllConnections();
  };
  process.once('SIGINT', stop);
  process.once('SIGTERM', stop);
}
