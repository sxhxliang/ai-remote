import { stopLocalStack } from './local-stack.mjs';

const stopped = await stopLocalStack();
console.log(stopped ? 'All local services have stopped.' : 'No local services are running.');
