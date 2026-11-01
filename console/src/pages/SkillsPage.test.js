import { fireEvent, render, screen } from '@testing-library/svelte';
import { beforeEach, expect, test, vi } from 'vitest';

vi.mock('../lib/api', () => ({
  api: {
    getSkills: vi.fn(),
    discoverSkills: vi.fn(),
    toggleSkill: vi.fn(),
    installSkill: vi.fn(),
    uninstallSkill: vi.fn()
  }
}));

import { api } from '../lib/api';
import SkillsPage from './SkillsPage.svelte';

beforeEach(() => {
  api.getSkills.mockReset();
  api.discoverSkills.mockReset();
  api.toggleSkill.mockReset();
  api.getSkills.mockResolvedValue({
    skills: [{ name: 'prx-skill', id: 'prx-skill', enabled: false, description: 'fixture', location: 'skills/prx-skill' }]
  });
});

test('toggles a skill and shows the returned fields', async () => {
  api.toggleSkill.mockResolvedValue({ status: 'ok', id: 'prx-skill', enabled: true });
  render(SkillsPage);
  await screen.findByRole('button', { name: 'Toggle prx-skill' });
  expect(screen.getByText(/does not publish a skill lifecycle/)).toBeInTheDocument();
  await fireEvent.click(screen.getByRole('button', { name: 'Toggle prx-skill' }));
  expect(await screen.findByText('Status ok, id prx-skill, enabled true.')).toBeInTheDocument();
  expect(api.toggleSkill).toHaveBeenCalledWith('prx-skill');
});

test('shows a denied toggle', async () => {
  const error = new Error('toggle refused');
  error.status = 403;
  api.toggleSkill.mockRejectedValue(error);
  render(SkillsPage);
  await fireEvent.click(await screen.findByRole('button', { name: 'Toggle prx-skill' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('toggle refused');
});

test('shows ClawHub as unavailable and does not search', async () => {
  render(SkillsPage);
  await screen.findByRole('button', { name: 'Installed' });
  await fireEvent.click(screen.getByRole('button', { name: 'Discover' }));
  await fireEvent.change(screen.getByRole('combobox'), { target: { value: 'clawhub' } });
  expect(await screen.findByText(/empty catalog is not a search result/)).toBeInTheDocument();
  expect(api.discoverSkills).not.toHaveBeenCalled();
});
