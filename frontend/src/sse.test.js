import test from 'node:test';
import assert from 'node:assert/strict';
import { readSse } from './sse.js';

test('reads OpenAI chat chunks across UTF-8 and SSE frame boundaries', async () => {
  const bytes = new TextEncoder().encode('data: {"choices":[{"delta":{"content":"你好🌍"},"finish_reason":null}]}\r\n\r\ndata: {"choices":[{"delta":{},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n');
  const stream = new ReadableStream({
    start(controller) {
      for (let index = 0; index < bytes.length; index += 2) controller.enqueue(bytes.slice(index, index + 2));
      controller.close();
    },
  });
  const chunks = [];
  for await (const chunk of readSse(stream)) chunks.push(chunk);
  assert.equal(chunks[0].choices[0].delta.content, '你好🌍');
  assert.equal(chunks[1].choices[0].finish_reason, 'stop');
  assert.deepEqual(chunks[2], { done: true });
});
