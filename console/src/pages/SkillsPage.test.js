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
  api.installSkill.mockReset();
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

const results = [
  {name:'first-skill',url:'https://github.com/example/first-skill',description:'First fixture',owner:'prx_user',stars:42,language:'Rust',has_license:true,source:'GitHub'},
  {name:'second-skill',url:'https://github.com/example/second-skill',description:'Second fixture',owner:'prx_node',stars:0,language:'Shell',has_license:false,source:'GitHub'}
];
async function search() {
  await fireEvent.click(await screen.findByRole('button', {name:'Discover'}));
  await fireEvent.click(screen.getByRole('button', {name:'Search'}));
}
async function secondInstall() {
  await screen.findByText('second-skill');
  await fireEvent.click(screen.getAllByRole('button',{name:'Install'})[1]);
}
test('r1e empty query defaults to agent skill and renders discovery metadata', async () => {
  api.discoverSkills.mockResolvedValue({results});
  render(SkillsPage); await search();
  expect(await screen.findByText('first-skill')).toBeInTheDocument();
  expect(api.discoverSkills).toHaveBeenCalledWith('github','agent skill');
  for (const text of ['First fixture','42','Rust','Licensed','No license']) expect(screen.getByText(text)).toBeInTheDocument();
  expect(screen.getByText(/prx_user/)).toBeInTheDocument();
  expect(screen.getByRole('link',{name:'example/first-skill'})).toHaveAttribute('href',results[0].url);
});
test('r6 selected install requires confirmation, cancels and disables pending confirmation', async () => {
  api.discoverSkills.mockResolvedValue({results});
  api.installSkill.mockReturnValue(new Promise(() => {}));
  render(SkillsPage); await search(); await secondInstall();
  expect(api.installSkill).not.toHaveBeenCalled();
  const dialog = screen.getByRole('dialog');
  expect(dialog).toHaveTextContent(results[1].url);
  expect(dialog).toHaveTextContent('Third-party, untrusted code. Stars: 0. License: No license.');
  expect(api.installSkill).not.toHaveBeenCalled();
  await fireEvent.click(screen.getByRole('button',{name:'Cancel'}));
  expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  await secondInstall(); await fireEvent.keyDown(document,{key:'Escape'});
  expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  expect(api.installSkill).not.toHaveBeenCalled();
  await secondInstall();
  const confirm = screen.getByRole('dialog').querySelector('.ui-dialog-confirm');
  await fireEvent.click(confirm);
  expect(api.installSkill).toHaveBeenCalledExactlyOnceWith(results[1].url,results[1].name);
  expect(confirm).toBeDisabled(); await fireEvent.click(confirm);
  expect(api.installSkill).toHaveBeenCalledTimes(1);
});
test('r8f installation preserves server inactive state and review guidance', async () => {
  api.discoverSkills.mockResolvedValue({results});
  api.installSkill.mockResolvedValue({status:'ok',skill:{name:results[1].name,description:'Second fixture',location:'skills/second-skill',enabled:false}});
  render(SkillsPage); await search(); await secondInstall();
  await fireEvent.click(screen.getByRole('dialog').querySelector('.ui-dialog-confirm'));
  await screen.findByText('Installed as inactive. Review it, then enable it in the Installed tab.');
  await fireEvent.click(screen.getByRole('button',{name:'Installed'}));
  const card = (await screen.findByText('second-skill')).closest('article');
  expect(card).toHaveTextContent('Disabled');
  await fireEvent.click(screen.getByRole('button',{name:'Discover'}));
  expect(await screen.findByText('Installed (inactive)')).toBeInTheDocument();
});
test.each([
  [429,'rate_limited','GitHub search rate limit reached; try again later'],
  [502,'upstream_error','GitHub search failed: invalid response']
])('r7c discovery error %s shows reason without empty state', async (status,code,reason) => {
  api.discoverSkills.mockRejectedValue(Object.assign(new Error(reason),{status,body:{code,error:reason}}));
  render(SkillsPage); await search();
  expect(await screen.findByText(new RegExp(reason))).toBeInTheDocument();
  if (code === 'rate_limited') expect(screen.getByText(/GitHub rate limit reached\./)).toBeInTheDocument();
  expect(screen.queryByText('No results found.')).not.toBeInTheDocument();
});
test('r7c valid empty discovery shows empty state', async () => {
  api.discoverSkills.mockResolvedValue({results:[]});
  render(SkillsPage); await search();
  expect(await screen.findByText('No results found.')).toBeInTheDocument();
});
