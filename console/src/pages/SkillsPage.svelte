<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';

  // --- State ---
  let activeTab = $state('installed');
  let skills = $state([]);
  let loading = $state(true);
  let errorMessage = $state('');
  let toast = $state('');
  let toastType = $state('success');

  // Discover state
  let discoverResults = $state([]);
  let discoverLoading = $state(false);
  let discoverQuery = $state('');
  let discoverSource = $state('github');
  let discoverSearched = $state(false);

  // Action state
  let installingUrl = $state('');
  let uninstallingName = $state('');
  let confirmUninstall = $state('');

  // --- Toast ---
  function showToast(msg, type = 'success') {
    toast = msg;
    toastType = type;
    setTimeout(() => { toast = ''; }, 3000);
  }

  // --- Installed skills ---
  async function loadSkills() {
    try {
      const response = await api.getSkills();
      skills = Array.isArray(response?.skills) ? response.skills : [];
      errorMessage = '';
    } catch {
      skills = [];
      errorMessage = 'Failed to load skills.';
    } finally {
      loading = false;
    }
  }

  async function toggleSkill(skillName) {
    try {
      await api.toggleSkill(skillName);
      skills = skills.map((s) =>
        s.name === skillName ? { ...s, enabled: !s.enabled } : s
      );
    } catch {
      skills = skills.map((s) =>
        s.name === skillName ? { ...s, enabled: !s.enabled } : s
      );
    }
  }

  async function handleUninstall(name) {
    if (confirmUninstall !== name) {
      confirmUninstall = name;
      return;
    }
    confirmUninstall = '';
    uninstallingName = name;
    try {
      await api.uninstallSkill(name);
      skills = skills.filter((s) => s.name !== name);
      showToast(t('skills.uninstallSuccess'));
    } catch (e) {
      showToast(t('skills.uninstallFailed') + (e.message ? `: ${e.message}` : ''), 'error');
    } finally {
      uninstallingName = '';
    }
  }

  // Sorted: enabled first
  const sortedSkills = $derived(
    [...skills].sort((a, b) => (a.enabled === b.enabled ? 0 : a.enabled ? -1 : 1))
  );
  const enabledCount = $derived(skills.filter((s) => s.enabled).length);

  // --- Discover ---
  async function searchSkills() {
    if (!discoverQuery.trim() && discoverSource === 'github') {
      discoverQuery = 'agent skill';
    }
    discoverLoading = true;
    discoverSearched = true;
    try {
      const response = await api.discoverSkills(discoverSource, discoverQuery);
      discoverResults = Array.isArray(response?.results) ? response.results : [];
    } catch {
      discoverResults = [];
    } finally {
      discoverLoading = false;
    }
  }

  function isInstalled(name) {
    return skills.some((s) => s.name === name);
  }

  async function installSkill(url, name) {
    installingUrl = url;
    try {
      const response = await api.installSkill(url, name);
      if (response?.skill) {
        skills = [...skills, { ...response.skill, enabled: true }];
      }
      showToast(t('skills.installSuccess'));
    } catch (e) {
      showToast(t('skills.installFailed') + (e.message ? `: ${e.message}` : ''), 'error');
    } finally {
      installingUrl = '';
    }
  }

  function handleSearchKeydown(e) {
    if (e.key === 'Enter') searchSkills();
  }

  // --- Init ---
  $effect(() => {
    loadSkills();
  });
</script>

<section class="space-y-6">
  <!-- Header -->
  <div class="flex items-center justify-between">
    <div class="flex items-center gap-3">
      <h2 class="text-2xl font-semibold">{t('skills.title')}</h2>
      {#if !loading && skills.length > 0}
        <span class="text-sm text-gray-500 dark:text-gray-400">
          {enabledCount}/{skills.length} {t('skills.active')}
        </span>
      {/if}
    </div>
    <button
      type="button"
      onclick={() => { loading = true; loadSkills(); }}
      class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
    >
      {t('common.refresh')}
    </button>
  </div>

  <!-- Tabs -->
  <div class="flex gap-1 rounded-lg border border-gray-200 bg-gray-100/50 p-1 dark:border-gray-700 dark:bg-gray-800/50">
    <button
      type="button"
      onclick={() => { activeTab = 'installed'; }}
      class="rounded-md px-4 py-2 text-sm font-medium transition {activeTab === 'installed' ? 'bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white' : 'text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200'}"
    >
      {t('skills.tabInstalled')}
    </button>
    <button
      type="button"
      onclick={() => { activeTab = 'discover'; }}
      class="rounded-md px-4 py-2 text-sm font-medium transition {activeTab === 'discover' ? 'bg-white text-gray-900 shadow-sm dark:bg-gray-700 dark:text-white' : 'text-gray-500 hover:text-gray-700 dark:text-gray-400 dark:hover:text-gray-200'}"
    >
      {t('skills.tabDiscover')}
    </button>
  </div>

  <!-- Toast -->
  {#if toast}
    <div class="rounded-lg px-4 py-2 text-sm {toastType === 'error' ? 'border border-red-500/30 bg-red-500/10 text-red-600 dark:text-red-300' : 'border border-green-500/30 bg-green-500/10 text-green-700 dark:text-green-300'}">
      {toast}
    </div>
  {/if}

  <!-- Installed Tab -->
  {#if activeTab === 'installed'}
    {#if loading}
      <p class="text-sm text-gray-500 dark:text-gray-400">{t('skills.loading')}</p>
    {:else if errorMessage}
      <p class="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-2 text-sm text-red-600 dark:text-red-300">
        {errorMessage}
      </p>
    {:else if skills.length === 0}
      <p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300">
        {t('skills.noSkills')}
      </p>
    {:else}
      <div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {#each sortedSkills as skill}
          <article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
            <div class="flex items-start justify-between gap-3">
              <h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100">{skill.name}</h3>
              <button type="button" onclick={() => toggleSkill(skill.name)}
                class="relative inline-flex h-5 w-9 flex-shrink-0 items-center rounded-full transition {skill.enabled ? 'bg-sky-600' : 'bg-gray-400 dark:bg-gray-600'}">
                <span class="inline-block h-3.5 w-3.5 rounded-full bg-white transition {skill.enabled ? 'translate-x-4' : 'translate-x-1'}"></span>
              </button>
            </div>
            {#if skill.description}
              <p class="mt-2 text-sm text-gray-500 dark:text-gray-400">{skill.description}</p>
            {/if}
            <p class="mt-2 font-mono text-xs text-gray-400 dark:text-gray-500">{skill.location}</p>
            <div class="mt-3 flex items-center justify-between">
              <span
                class="rounded-full px-2 py-1 text-xs font-medium {skill.enabled
                  ? 'border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300'
                  : 'border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300'}"
              >
                {skill.enabled ? t('common.enabled') : t('common.disabled')}
              </span>
              {#if confirmUninstall === skill.name}
                <div class="flex items-center gap-2">
                  <span class="text-xs text-yellow-600 dark:text-yellow-400">{t('skills.confirmUninstall').replace('{name}', skill.name)}</span>
                  <button
                    type="button"
                    onclick={() => handleUninstall(skill.name)}
                    disabled={uninstallingName === skill.name}
                    class="rounded px-2 py-1 text-xs font-medium text-red-500 transition hover:bg-red-500/20 disabled:opacity-50 dark:text-red-400"
                  >
                    {uninstallingName === skill.name ? t('skills.uninstalling') : t('common.yes')}
                  </button>
                  <button
                    type="button"
                    onclick={() => { confirmUninstall = ''; }}
                    class="rounded px-2 py-1 text-xs text-gray-500 transition hover:bg-gray-200 dark:text-gray-400 dark:hover:bg-gray-700"
                  >
                    {t('common.no')}
                  </button>
                </div>
              {:else}
                <button
                  type="button"
                  onclick={() => handleUninstall(skill.name)}
                  class="rounded px-2 py-1 text-xs text-red-500 transition hover:bg-red-500/20 dark:text-red-400"
                >
                  {t('skills.uninstall')}
                </button>
              {/if}
            </div>
          </article>
        {/each}
      </div>
    {/if}
  {/if}

  <!-- Discover Tab -->
  {#if activeTab === 'discover'}
    <div class="flex flex-col gap-3 sm:flex-row">
      <select
        bind:value={discoverSource}
        class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200"
      >
        <option value="github">GitHub</option>
        <option value="clawhub">ClawHub</option>
        <option value="huggingface">HuggingFace</option>
      </select>
      <input
        type="text"
        bind:value={discoverQuery}
        onkeydown={handleSearchKeydown}
        placeholder={t('skills.search')}
        class="flex-1 rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 placeholder-gray-400 focus:border-sky-500 focus:outline-none dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:placeholder-gray-500"
      />
      <button
        type="button"
        onclick={searchSkills}
        disabled={discoverLoading}
        class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"
      >
        {discoverLoading ? t('skills.searching') : t('skills.searchBtn')}
      </button>
    </div>

    {#if discoverLoading}
      <p class="text-sm text-gray-500 dark:text-gray-400">{t('skills.searching')}</p>
    {:else if discoverSearched && discoverResults.length === 0}
      <p class="rounded-xl border border-gray-200 bg-white px-4 py-3 text-sm text-gray-600 dark:border-gray-700 dark:bg-gray-800 dark:text-gray-300">
        {t('skills.noResults')}
      </p>
    {:else if discoverResults.length > 0}
      <div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {#each discoverResults as result}
          {@const alreadyInstalled = isInstalled(result.name)}
          <article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
            <div class="flex items-start justify-between gap-2">
              <div class="min-w-0 flex-1">
                <h3 class="truncate text-lg font-semibold text-gray-900 dark:text-gray-100">{result.name}</h3>
                <p class="text-xs text-gray-400 dark:text-gray-500">{t('skills.owner')} {result.owner}</p>
              </div>
              <span class="rounded-full border border-gray-300 bg-gray-100 px-2 py-0.5 text-xs text-gray-600 dark:border-gray-600 dark:bg-gray-700 dark:text-gray-300">
                {result.source}
              </span>
            </div>
            {#if result.description}
              <p class="mt-2 line-clamp-3 text-sm text-gray-500 dark:text-gray-400">{result.description}</p>
            {/if}
            <div class="mt-3 flex flex-wrap items-center gap-2 text-xs text-gray-400 dark:text-gray-500">
              {#if result.stars > 0}
                <span class="flex items-center gap-1">
                  <svg class="h-3.5 w-3.5" fill="currentColor" viewBox="0 0 20 20"><path d="M9.049 2.927c.3-.921 1.603-.921 1.902 0l1.07 3.292a1 1 0 00.95.69h3.462c.969 0 1.371 1.24.588 1.81l-2.8 2.034a1 1 0 00-.364 1.118l1.07 3.292c.3.921-.755 1.688-1.54 1.118l-2.8-2.034a1 1 0 00-1.175 0l-2.8 2.034c-.784.57-1.838-.197-1.539-1.118l1.07-3.292a1 1 0 00-.364-1.118L2.98 8.72c-.783-.57-.38-1.81.588-1.81h3.461a1 1 0 00.951-.69l1.07-3.292z"/></svg>
                  {result.stars}
                </span>
              {/if}
              {#if result.language}
                <span class="rounded bg-gray-100 px-1.5 py-0.5 dark:bg-gray-700">{result.language}</span>
              {/if}
              <span class="{result.has_license ? 'text-green-600 dark:text-green-400' : 'text-yellow-600 dark:text-yellow-400'}">
                {result.has_license ? t('skills.licensed') : t('skills.unlicensed')}
              </span>
            </div>
            <div class="mt-3 flex items-center justify-between">
              <a
                href={result.url}
                target="_blank"
                rel="noopener noreferrer"
                class="text-xs text-sky-600 hover:underline dark:text-sky-400"
              >
                {result.url.replace('https://github.com/', '')}
              </a>
              {#if alreadyInstalled}
                <span class="rounded-full border border-green-500/50 bg-green-500/20 px-2 py-1 text-xs font-medium text-green-700 dark:text-green-300">
                  {t('skills.installed')}
                </span>
              {:else}
                <button
                  type="button"
                  onclick={() => installSkill(result.url, result.name)}
                  disabled={installingUrl === result.url}
                  class="rounded-lg bg-sky-600 px-3 py-1 text-xs font-medium text-white transition hover:bg-sky-500 disabled:opacity-50"
                >
                  {installingUrl === result.url ? t('skills.installing') : t('skills.install')}
                </button>
              {/if}
            </div>
          </article>
        {/each}
      </div>
    {/if}
  {/if}
</section>
