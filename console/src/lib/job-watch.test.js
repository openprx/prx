import { afterEach, expect, test, vi } from 'vitest';
import { createJobWatch } from './job-watch';

afterEach(() => {
  vi.useRealTimers();
});

test('polls immediately, backs off, and stops on a terminal phase', async () => {
  vi.useFakeTimers();
  const statuses = ['running', 'running', 'succeeded'];
  const seen = [];
  const watch = createJobWatch({
    poll: async () => ({ status: statuses[seen.length] || 'succeeded', elapsed_ms: seen.length }),
    onUpdate(update) {
      seen.push(update.snapshot.status);
    }
  });
  await vi.waitFor(() => expect(seen).toEqual(['running']));
  await vi.advanceTimersByTimeAsync(1000);
  await vi.waitFor(() => expect(seen).toEqual(['running', 'running']));
  await vi.advanceTimersByTimeAsync(1500);
  await vi.waitFor(() => expect(seen).toEqual(['running', 'running', 'succeeded']));
  await vi.advanceTimersByTimeAsync(8000);
  expect(seen).toHaveLength(3);
  watch.stop();
});

test('retries a failed poll read and does not treat it as terminal', async () => {
  vi.useFakeTimers();
  let calls = 0;
  const watch = createJobWatch({
    poll: async () => {
      calls += 1;
      if (calls === 1) {
        throw new TypeError('network');
      }
      return { status: 'running', elapsed_ms: 1 };
    },
    onUpdate() {}
  });
  await vi.waitFor(() => expect(calls).toBe(1));
  await vi.advanceTimersByTimeAsync(1000);
  await vi.waitFor(() => expect(calls).toBe(2));
  watch.stop();
});
