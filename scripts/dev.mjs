import { startLocalStack } from './local-stack.mjs';

const portIndex = process.argv.indexOf('--mock-port');
const mockPort = portIndex >= 0 ? Number(process.argv[portIndex + 1]) : 11434;
const stack = await startLocalStack({ build: !process.argv.includes('--no-build'), mockPort });
console.log('Local Mock Ollama is ready: ' + stack.http);
console.log('Connection settings are filled automatically. Click 连接 to chat.');
console.log('Mock Ollama: ' + stack.env.OLLAMA_BASE);
console.log('Stop all local services: node scripts/stop-local.mjs');
process.once('SIGINT', () => stack.stop().catch(console.error));
process.once('SIGTERM', () => stack.stop().catch(console.error));
