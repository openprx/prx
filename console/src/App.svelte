<script>
  import { api } from './lib/api';
  import { clearToken, getToken } from './lib/auth';
  import { buildConfigNavGroups, focusConfigSection } from './lib/config-nav';
  import { NAV_ITEMS } from './lib/constants';
  import { LANG_STORAGE_KEY, i18n, syncLanguageFromStorage, t, toggleLanguage } from './lib/i18n';
  import { currentPath, initRouter, navigate } from './lib/router';
  import { Sun, Moon } from '@lucide/svelte';

  import LoginPage from './pages/LoginPage.svelte';
  import OverviewPage from './pages/OverviewPage.svelte';
  import SessionsPage from './pages/SessionsPage.svelte';
  import ChatPage from './pages/ChatPage.svelte';
  import ChannelsPage from './pages/ChannelsPage.svelte';
  import ConfigPage from './pages/ConfigPage.svelte';
  import LogsPage from './pages/LogsPage.svelte';
  import HooksPage from './pages/HooksPage.svelte';
  import McpPage from './pages/McpPage.svelte';
  import SkillsPage from './pages/SkillsPage.svelte';
  import PluginsPage from './pages/PluginsPage.svelte';

  let path = $state(currentPath());
  let token = $state(getToken());
  let mobileSidebarOpen = $state(false);
  let isDark = $state(true);
  let configNavGroups = $state([]);
  let configNavLoading = $state(false);
  let activeConfigHash = $state(typeof window !== 'undefined' ? window.location.hash : '');

  const isAuthenticated = $derived(token.length > 0);
  const activePath = $derived(isAuthenticated && path === '/' ? '/overview' : path);
  const activeNavPath = $derived(activePath.startsWith('/chat/') ? '/sessions' : activePath);
  const showConfigSubnav = $derived(activePath === '/config');
  const activeConfigSection = $derived(
    activeConfigHash.startsWith('#config-section-')
      ? activeConfigHash.slice('#config-section-'.length)
      : ''
  );
  function safeDecodeSessionId(rawValue) {
    try {
      return decodeURIComponent(rawValue);
    } catch {
      return rawValue;
    }
  }
  const chatSessionId = $derived(
    activePath.startsWith('/chat/') ? safeDecodeSessionId(activePath.slice('/chat/'.length)) : ''
  );

  function initTheme() {
    const saved = localStorage.getItem('prx-console-theme');
    if (saved === 'light') {
      isDark = false;
    } else {
      isDark = true;
    }
    applyTheme();
  }

  function applyTheme() {
    if (isDark) {
      document.documentElement.classList.add('dark');
    } else {
      document.documentElement.classList.remove('dark');
    }
  }

  function toggleTheme() {
    isDark = !isDark;
    localStorage.setItem('prx-console-theme', isDark ? 'dark' : 'light');
    applyTheme();
  }

  function refreshToken() {
    token = getToken();
  }

  function onRouteChange(nextPath) {
    path = nextPath;
    mobileSidebarOpen = false;
    activeConfigHash = typeof window !== 'undefined' ? window.location.hash : '';
  }

  function onLogin(nextToken) {
    token = nextToken;
    navigate('/overview', true);
  }

  function logout() {
    clearToken();
    token = '';
    navigate('/', true);
  }

  function goTo(targetPath) {
    navigate(targetPath);
  }

  function onHashChange() {
    activeConfigHash = window.location.hash;
  }

  async function loadConfigNavGroups() {
    if (!isAuthenticated || activePath !== '/config' || configNavLoading) return;

    configNavLoading = true;
    try {
      const config = await api.getConfig();
      configNavGroups = buildConfigNavGroups(config);
    } catch {
      configNavGroups = buildConfigNavGroups(null);
    } finally {
      configNavLoading = false;
    }
  }

  function goToConfigSection(groupKey) {
    focusConfigSection(groupKey);
    mobileSidebarOpen = false;
  }

  $effect(() => {
    initTheme();

    const stopRouter = initRouter(onRouteChange);
    const onStorage = (event) => {
      if (event.key === 'prx-console-token') {
        refreshToken();
        return;
      }

      if (event.key === LANG_STORAGE_KEY) {
        syncLanguageFromStorage();
      }

      if (event.key === 'prx-console-theme') {
        const saved = localStorage.getItem('prx-console-theme');
        isDark = saved !== 'light';
        applyTheme();
      }
    };

    window.addEventListener('storage', onStorage);
    window.addEventListener('hashchange', onHashChange);

    return () => {
      stopRouter();
      window.removeEventListener('storage', onStorage);
      window.removeEventListener('hashchange', onHashChange);
    };
  });

  $effect(() => {
    if (isAuthenticated && path === '/') {
      navigate('/overview', true);
      return;
    }

    if (!isAuthenticated && path !== '/') {
      navigate('/', true);
    }
  });

  $effect(() => {
    if (showConfigSubnav) {
      loadConfigNavGroups();
      return;
    }

    configNavGroups = [];
  });
</script>

<div class="min-h-screen bg-gray-50 text-gray-900 dark:bg-gray-900 dark:text-gray-100">
  {#if !isAuthenticated}
    <LoginPage onLogin={onLogin} />
  {:else}
    <div class="flex min-h-screen">
      {#if mobileSidebarOpen}
        <button
          type="button"
          aria-label={t('app.closeSidebar')}
          class="fixed inset-0 z-30 bg-black/30 dark:bg-black/60 lg:hidden"
          onclick={() => (mobileSidebarOpen = false)}
        ></button>
      {/if}

      <aside
        class={`fixed inset-y-0 left-0 z-40 w-64 border-r border-gray-200 bg-white p-4 transition-transform dark:border-gray-700 dark:bg-gray-800 lg:static lg:translate-x-0 ${
          mobileSidebarOpen ? 'translate-x-0' : '-translate-x-full'
        }`}
      >
        <div class="mb-4 border-b border-gray-200 pb-4 dark:border-gray-700">
          <p class="text-lg font-semibold">{t('app.title')}</p>
        </div>

        <nav class="space-y-1">
          {#each NAV_ITEMS as item}
            <button
              type="button"
              onclick={() => goTo(item.path)}
              class={`w-full rounded-lg px-3 py-2 text-left text-sm transition ${
                activeNavPath === item.path
                  ? 'bg-sky-600 text-white'
                  : 'text-gray-600 hover:bg-gray-100 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-gray-100'
              }`}
            >
              {t(item.labelKey)}
            </button>

            {#if item.path === '/config' && showConfigSubnav}
              <div class="ml-4 mt-1 space-y-1 border-l border-gray-200 pl-3 dark:border-gray-700">
                {#each configNavGroups as group}
                  <button
                    type="button"
                    onclick={() => goToConfigSection(group.groupKey)}
                    class={`w-full rounded-md px-2 py-1.5 text-left text-xs transition ${
                      activeConfigSection === group.groupKey
                        ? 'bg-sky-50 text-sky-700 dark:bg-sky-500/10 dark:text-sky-300'
                        : 'text-gray-500 hover:bg-gray-100 hover:text-gray-800 dark:text-gray-400 dark:hover:bg-gray-700 dark:hover:text-gray-100'
                    }`}
                  >
                    {group.label}
                  </button>
                {/each}

                {#if configNavLoading && configNavGroups.length === 0}
                  <p class="px-2 py-1 text-xs text-gray-400 dark:text-gray-500">Loading...</p>
                {/if}
              </div>
            {/if}
          {/each}
        </nav>
      </aside>

      <div class="flex min-w-0 flex-1 flex-col">
        <header class="sticky top-0 z-20 flex items-center justify-between border-b border-gray-200 bg-white/95 px-4 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95">
          <div class="flex items-center gap-3">
            <button
              type="button"
              class="rounded-lg border border-gray-300 px-2 py-1 text-sm text-gray-700 dark:border-gray-700 dark:text-gray-200 lg:hidden"
              onclick={() => (mobileSidebarOpen = !mobileSidebarOpen)}
            >
              {t('app.menu')}
            </button>
            <h1 class="text-lg font-semibold">{t('app.title')}</h1>
          </div>

          <div class="flex items-center gap-2">
            <button
              type="button"
              aria-label="Toggle theme"
              onclick={toggleTheme}
              class="rounded-lg border border-gray-300 bg-white p-2 text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
            >
              {#if isDark}
                <Sun size={16} />
              {:else}
                <Moon size={16} />
              {/if}
            </button>
            <button
              type="button"
              aria-label={t('app.language')}
              onclick={toggleLanguage}
              class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
            >
              {i18n.lang === 'zh' ? '中文 / EN' : 'EN / 中文'}
            </button>
            <button
              type="button"
              onclick={logout}
              class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
            >
              {t('common.logout')}
            </button>
          </div>
        </header>

        <main class="flex-1 p-4 sm:p-6">
          {#if activePath === '/overview'}
            <OverviewPage />
          {:else if activePath === '/sessions'}
            <SessionsPage />
          {:else if activePath.startsWith('/chat/')}
            <ChatPage sessionId={chatSessionId} />
          {:else if activePath === '/channels'}
            <ChannelsPage />
          {:else if activePath === '/hooks'}
            <HooksPage />
          {:else if activePath === '/mcp'}
            <McpPage />
          {:else if activePath === '/skills'}
            <SkillsPage />
          {:else if activePath === '/plugins'}
            <PluginsPage />
          {:else if activePath === '/config'}
            <ConfigPage />
          {:else if activePath === '/logs'}
            <LogsPage />
          {:else}
            <section class="space-y-4">
              <h2 class="text-2xl font-semibold">{t('app.notFound')}</h2>
              <button
                type="button"
                onclick={() => goTo('/overview')}
                class="rounded-lg bg-sky-600 px-3 py-2 text-sm font-medium text-white hover:bg-sky-500"
              >
                {t('app.backToOverview')}
              </button>
            </section>
          {/if}
        </main>
      </div>
    </div>
  {/if}
</div>
