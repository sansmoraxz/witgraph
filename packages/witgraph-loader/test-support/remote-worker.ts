// A worker serving sessions, for `test/remote.test.ts`.
import { workerData } from 'node:worker_threads';
import { serveSession } from '../src/index.ts';

serveSession(workerData.port);
