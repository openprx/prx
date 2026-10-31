import { ApiError } from './api';
import { STILL_RUNNING_MS, isTerminalPhase, pollDelayMs } from './transport';

// Poll a job that the server has already accepted. The POST that created the
// job is the caller's responsibility and is never repeated here.
export function createJobWatch({ poll, onUpdate }) {
  let stopped = false;
  let running = false;
  let restart = false;
  let attempt = 0;
  let timer = 0;
  const started = Date.now();

  async function tick() {
    if (stopped || running) {
      return;
    }
    running = true;
    let terminal = false;
    try {
      const snapshot = await poll();
      const watchMs = Date.now() - started;
      const status = snapshot && typeof snapshot.status === 'string' ? snapshot.status : '';
      terminal = isTerminalPhase(status);
      onUpdate({
        snapshot,
        error: null,
        stillRunning: !terminal && watchMs >= STILL_RUNNING_MS,
        terminalUnknown: false
      });
    } catch (error) {
      const terminalUnknown = error instanceof ApiError && error.status === 404;
      terminal = terminalUnknown;
      onUpdate({ snapshot: null, error, stillRunning: false, terminalUnknown });
    } finally {
      running = false;
    }
    if (stopped || terminal) {
      stopped = true;
      return;
    }
    if (restart) {
      restart = false;
      attempt = 0;
      tick();
      return;
    }
    const delay = pollDelayMs(attempt);
    attempt += 1;
    timer = setTimeout(() => {
      tick();
    }, delay);
  }

  tick();

  return {
    stop() {
      stopped = true;
      restart = false;
      clearTimeout(timer);
    },
    checkAgain() {
      clearTimeout(timer);
      stopped = false;
      attempt = 0;
      if (running) {
        restart = true;
        return;
      }
      tick();
    }
  };
}
