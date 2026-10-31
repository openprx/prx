import { fireEvent, render, screen } from '@testing-library/svelte';
import { expect, test, vi } from 'vitest';
import TurnProgress from './TurnProgress.svelte';

test('shows the phase, elapsed time, and cancel action', async () => {
  const onCancel = vi.fn();
  render(TurnProgress, {
    props: {
      phase: 'running',
      elapsedMs: 1200,
      jobId: 'job-1',
      workId: 'work-1',
      onCancel
    }
  });
  expect(screen.getByText('Running')).toBeInTheDocument();
  expect(screen.getByText('Elapsed 1200 ms')).toBeInTheDocument();
  await fireEvent.click(screen.getByRole('button', { name: 'Cancel turn' }));
  expect(onCancel).toHaveBeenCalledOnce();
});

test('a missing job shows the history notice and check again, without cancel', async () => {
  const onCheckAgain = vi.fn();
  const onCancel = vi.fn();
  render(TurnProgress, {
    props: {
      phase: 'running',
      elapsedMs: 40,
      jobId: 'job-1',
      workId: 'work-1',
      jobMissing: true,
      onCheckAgain,
      onCancel
    }
  });
  expect(screen.getByText('The gateway no longer knows this job; check history.')).toBeInTheDocument();
  await fireEvent.click(screen.getByRole('button', { name: 'Check again' }));
  expect(onCheckAgain).toHaveBeenCalledOnce();
  expect(screen.queryByRole('button', { name: 'Cancel turn' })).not.toBeInTheDocument();
});

test('hides cancel when the caller does not provide onCancel', () => {
  render(TurnProgress, {
    props: {
      phase: 'running',
      elapsedMs: 40,
      jobId: 'job-1',
      workId: 'work-1'
    }
  });
  expect(screen.getByText('Running')).toBeInTheDocument();
  expect(screen.queryByRole('button', { name: 'Cancel turn' })).not.toBeInTheDocument();
});

test('a stale poll offers check again without a cancel on a finished phase', async () => {
  const onCheckAgain = vi.fn();
  render(TurnProgress, {
    props: {
      phase: 'succeeded',
      elapsedMs: 5,
      stillRunning: true,
      pollStale: true,
      onCheckAgain
    }
  });
  expect(screen.getByText('The progress check lost contact. The turn was not sent again.')).toBeInTheDocument();
  await fireEvent.click(screen.getByRole('button', { name: 'Check again' }));
  expect(onCheckAgain).toHaveBeenCalledOnce();
  expect(screen.queryByRole('button', { name: 'Cancel turn' })).not.toBeInTheDocument();
});
