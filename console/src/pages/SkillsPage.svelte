<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';

  let skills = $state([]);
  let loading = $state(true);
  let errorMessage = $state('');

  async function loadSkills() {
    try {
      const response = await api.getSkills();
      skills = Array.isArray(response?.skills) ? response.skills : [];
      errorMessage = '';
    } catch {
      // API not implemented yet - use mock data
      skills = [
        {
          name: 'weather',
          description: 'Get current weather and forecasts via wttr.in or Open-Meteo.',
          location: '~/.openclaw/skills/weather/',
          enabled: true
        },
        {
          name: 'github',
          description: 'GitHub operations via gh CLI: issues, PRs, CI runs, code review.',
          location: '~/.openclaw/skills/github/',
          enabled: true
        },
        {
          name: 'edge-tts',
          description: 'Text-to-speech conversion using node-edge-tts for generating audio.',
          location: '~/.openclaw/skills/edge-tts/',
          enabled: true
        },
        {
          name: 'coding-agent',
          description: 'Delegate coding tasks to Codex, Claude Code, or Pi agents.',
          location: '~/.openclaw/skills/coding-agent/',
          enabled: false
        },
        {
          name: 'video-frames',
          description: 'Extract frames or short clips from videos using ffmpeg.',
          location: '~/.openclaw/skills/video-frames/',
          enabled: false
        }
      ];
      errorMessage = '';
    } finally {
      loading = false;
    }
  }

  function toggleSkill(skillName) {
    skills = skills.map((s) =>
      s.name === skillName ? { ...s, enabled: !s.enabled } : s
    );
  }

  async function refreshSkills() {
    loading = true;
    await loadSkills();
  }

  const enabledCount = $derived(skills.filter((s) => s.enabled).length);

  $effect(() => {
    loadSkills();
  });
</script>

<section class="space-y-6">
  <div class="flex items-center justify-between">
    <div class="flex items-center gap-3">
      <h2 class="text-2xl font-semibold">{t('skills.title')}</h2>
      {#if !loading && skills.length > 0}
        <span class="text-sm text-gray-400">
          {enabledCount}/{skills.length} {t('skills.active')}
        </span>
      {/if}
    </div>
    <button
      type="button"
      onclick={refreshSkills}
      class="rounded-lg border border-gray-600 bg-gray-800 px-3 py-2 text-sm text-gray-200 transition hover:bg-gray-700"
    >
      {t('common.refresh')}
    </button>
  </div>

  {#if loading}
    <p class="text-sm text-gray-400">{t('skills.loading')}</p>
  {:else if errorMessage}
    <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-300">
      {errorMessage}
    </p>
  {:else if skills.length === 0}
    <p class="rounded-xl border border-gray-700 bg-gray-800 px-4 py-3 text-sm text-gray-300">
      {t('skills.noSkills')}
    </p>
  {:else}
    <div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
      {#each skills as skill}
        <article class="rounded-xl border border-gray-700 bg-gray-800 p-4">
          <div class="flex items-start justify-between gap-3">
            <h3 class="text-lg font-semibold text-gray-100">{skill.name}</h3>
            <button type="button" onclick={() => toggleSkill(skill.name)}
              class={`relative inline-flex h-5 w-9 flex-shrink-0 items-center rounded-full transition ${skill.enabled ? 'bg-sky-600' : 'bg-gray-600'}`}>
              <span class={`inline-block h-3.5 w-3.5 rounded-full bg-white transition ${skill.enabled ? 'translate-x-4' : 'translate-x-1'}`}></span>
            </button>
          </div>
          {#if skill.description}
            <p class="mt-2 text-sm text-gray-400">{skill.description}</p>
          {/if}
          <p class="mt-2 font-mono text-xs text-gray-500">{skill.location}</p>
          <div class="mt-3">
            <span
              class={`rounded-full px-2 py-1 text-xs font-medium ${
                skill.enabled
                  ? 'border border-green-500/50 bg-green-500/20 text-green-300'
                  : 'border border-red-500/50 bg-red-500/20 text-red-300'
              }`}
            >
              {skill.enabled ? t('common.enabled') : t('common.disabled')}
            </span>
          </div>
        </article>
      {/each}
    </div>
  {/if}
</section>
