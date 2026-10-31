<script>
  import { Sun, Moon } from '@lucide/svelte';
  import { api, apiBaseUrl } from './lib/api';
  import { clearToken, getToken, syncTokenCookie } from './lib/auth';
  import { gatewayOrigin } from './lib/context-status';
  import { NAV_GROUPS, activeNavTarget, resolveRedirect } from './lib/constants';
  import {
    LANG_STORAGE_KEY,
    SUPPORTED_LANGUAGES,
    i18n,
    setLanguage,
    syncLanguageFromStorage,
    t
  } from './lib/i18n';
  import { currentPath, initRouter, navigate } from './lib/router';
  import ContextHeader from './lib/ui/ContextHeader.svelte';
  import PageHeader from './lib/ui/PageHeader.svelte';

  import LoginPage from './pages/LoginPage.svelte';
  import OverviewPage from './pages/OverviewPage.svelte';
  import SessionsPage from './pages/SessionsPage.svelte';
  import DecisionPlanePage from './pages/DecisionPlanePage.svelte';
  import DecisionInspectPage from './pages/DecisionInspectPage.svelte';
  import ChatPage from './pages/ChatPage.svelte';
  import ChannelsPage from './pages/ChannelsPage.svelte';
  import ConfigPage from './pages/ConfigPage.svelte';
  import LogsPage from './pages/LogsPage.svelte';
  import HooksPage from './pages/HooksPage.svelte';
  import McpPage from './pages/McpPage.svelte';
  import SkillsPage from './pages/SkillsPage.svelte';
  import PluginsPage from './pages/PluginsPage.svelte';
  import ProvidersPage from './pages/ProvidersPage.svelte';
  import ModelsPage from './pages/ModelsPage.svelte';
  import RoutingPage from './pages/RoutingPage.svelte';
  import ChangesPage from './pages/ChangesPage.svelte';

  let path = $state(currentPath());
  let token = $state(getToken());
  let mobileSidebarOpen = $state(false);
  const THEME_STORAGE_KEY = 'prx-console-theme';
  let themePreference = $state('system');
  let isDark = $state(true);
  let gatewayStatus = $state(null);
  let groupOpen = $state({
    work: true,
    capabilities: true,
    automation: true,
    operations: true
  });

  const isAuthenticated = $derived(token.length > 0);
  const activePath = $derived(isAuthenticated && path === '/' ? '/overview' : path);
  const navTarget = $derived(activeNavTarget(activePath));
  const gatewayAddress = $derived(gatewayOrigin(apiBaseUrl));
  const languageOptions = $derived(
    SUPPORTED_LANGUAGES.map((lang) => ({
      value: lang,
      label: t(`languages.${lang}`)
    }))
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

  function resolvePreferredDark() {
    if (themePreference === 'dark') return true;
    if (themePreference === 'light') return false;
    return window.matchMedia('(prefers-color-scheme: dark)').matches;
  }

  function initTheme() {
    const saved = localStorage.getItem(THEME_STORAGE_KEY);
    themePreference = saved === 'light' || saved === 'dark' ? saved : 'system';
    applyTheme();
  }

  function applyTheme() {
    const nextIsDark = resolvePreferredDark();
    isDark = nextIsDark;
    document.documentElement.classList.toggle('dark', nextIsDark);
    document.documentElement.classList.toggle('light', !nextIsDark);
    document.documentElement.style.colorScheme = nextIsDark ? 'dark' : 'light';
  }

  function toggleTheme() {
    themePreference = isDark ? 'light' : 'dark';
    localStorage.setItem(THEME_STORAGE_KEY, themePreference);
    applyTheme();
  }

  function refreshToken() {
    token = getToken();
    syncTokenCookie();
  }

  function onRouteChange(nextPath) {
    path = nextPath;
    mobileSidebarOpen = false;
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

  function toggleGroup(groupId) {
    groupOpen[groupId] = groupOpen[groupId] === false;
  }

  $effect(() => {
    initTheme();
    syncTokenCookie();

    const stopRouter = initRouter(onRouteChange);
    const mediaQuery = window.matchMedia('(prefers-color-scheme: dark)');
    const onStorage = (event) => {
      if (event.key === 'prx-console-token') {
        refreshToken();
        return;
      }

      if (event.key === LANG_STORAGE_KEY) {
        syncLanguageFromStorage();
      }

      if (event.key === THEME_STORAGE_KEY) {
        const saved = localStorage.getItem(THEME_STORAGE_KEY);
        themePreference = saved === 'light' || saved === 'dark' ? saved : 'system';
        applyTheme();
      }
    };
    const onThemeMediaChange = () => {
      if (themePreference === 'system') {
        applyTheme();
      }
    };
    const onKeyDown = (event) => {
      if (event.key === 'Escape' && mobileSidebarOpen) {
        mobileSidebarOpen = false;
      }
    };

    window.addEventListener('storage', onStorage);
    window.addEventListener('keydown', onKeyDown);
    mediaQuery.addEventListener('change', onThemeMediaChange);

    return () => {
      stopRouter();
      window.removeEventListener('storage', onStorage);
      window.removeEventListener('keydown', onKeyDown);
      mediaQuery.removeEventListener('change', onThemeMediaChange);
    };
  });

  $effect(() => {
    if (isAuthenticated && path === '/') {
      navigate('/overview', true);
      return;
    }

    if (!isAuthenticated && path !== '/') {
      navigate('/', true);
      return;
    }

    if (isAuthenticated) {
      const redirected = resolveRedirect(path);
      if (redirected) {
        navigate(redirected, true);
      }
    }
  });

  $effect(() => {
    if (!isAuthenticated) {
      gatewayStatus = null;
      return undefined;
    }

    let cancelled = false;
    const refresh = async () => {
      try {
        const next = await api.getStatus();
        if (!cancelled) {
          gatewayStatus = next && typeof next === 'object' ? next : null;
        }
      } catch {
        if (!cancelled) {
          gatewayStatus = null;
        }
      }
    };

    refresh();
    const timer = setInterval(refresh, 30_000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  });
</script>

<div class="console-shell min-h-screen bg-gray-50 text-gray-900 dark:bg-gray-900 dark:text-gray-100">
  <a class="ui-skip-link" href="#console-main">{t('shell.skipToContent')}</a>
  {#if !isAuthenticated}
    <main id="console-main" class="min-h-screen" tabindex="-1">
      <LoginPage onLogin={onLogin} />
    </main>
  {:else}
    <div class="flex min-h-screen max-w-full">
      {#if mobileSidebarOpen}
        <button
          type="button"
          aria-label={t('app.closeSidebar')}
          class="fixed inset-0 z-30 bg-black/30 dark:bg-black/60 lg:hidden"
          onclick={() => (mobileSidebarOpen = false)}
        ></button>
      {/if}

      <aside
        id="console-sidebar"
        class={`console-sidebar fixed inset-y-0 left-0 z-40 w-64 overflow-y-auto border-r border-gray-200 bg-white p-4 transition-transform dark:border-gray-700 dark:bg-gray-800 lg:static lg:translate-x-0 ${
          mobileSidebarOpen ? 'translate-x-0' : '-translate-x-full'
        }`}
      >
        <div class="mb-4 border-b border-gray-200 pb-4 dark:border-gray-700">
          <p class="text-lg font-semibold">{t('app.title')}</p>
        </div>

        <nav class="space-y-3" aria-label={t('shell.navLabel')}>
          {#each NAV_GROUPS as group (group.id)}
            <section class={`ui-nav-group ${navTarget?.groupId === group.id ? 'is-current' : ''}`}>
              <button
                type="button"
                class="ui-nav-group-toggle flex w-full items-center justify-between rounded-md px-2 py-1 text-left text-xs font-semibold uppercase tracking-wide text-gray-500 hover:text-gray-900 dark:text-gray-400 dark:hover:text-gray-100"
                aria-expanded={groupOpen[group.id] !== false}
                aria-controls={`nav-group-${group.id}`}
                aria-current={navTarget?.groupId === group.id ? 'true' : undefined}
                onclick={() => toggleGroup(group.id)}
              >
                {t(group.labelKey)}
              </button>
              {#if groupOpen[group.id] !== false}
                <ul id={`nav-group-${group.id}`} class="mt-1 space-y-1">
                  {#each group.items as item (item.path)}
                    <li>
                      <button
                        type="button"
                        onclick={() => goTo(item.path)}
                        aria-current={navTarget?.path === item.path ? 'page' : undefined}
                        class={`w-full rounded-lg px-3 py-2 text-left text-sm transition ${
                          navTarget?.path === item.path
                            ? 'bg-sky-600 text-white'
                            : 'text-gray-600 hover:bg-gray-100 hover:text-gray-900 dark:text-gray-300 dark:hover:bg-gray-700 dark:hover:text-gray-100'
                        }`}
                      >
                        {t(item.labelKey)}
                      </button>
                    </li>
                  {/each}
                </ul>
              {/if}
            </section>
          {/each}
        </nav>
      </aside>

      <div class="flex min-w-0 max-w-full flex-1 flex-col">
        <header class="console-header sticky top-0 z-20 flex max-w-full flex-wrap items-center justify-between gap-2 border-b border-gray-200 bg-white/95 px-4 py-3 backdrop-blur dark:border-gray-700 dark:bg-gray-900/95">
          <div class="flex min-w-0 flex-1 flex-wrap items-center gap-2">
            <button
              type="button"
              class="rounded-lg border border-gray-300 px-2 py-1 text-sm text-gray-700 dark:border-gray-700 dark:text-gray-200 lg:hidden"
              aria-expanded={mobileSidebarOpen}
              aria-controls="console-sidebar"
              onclick={() => (mobileSidebarOpen = !mobileSidebarOpen)}
            >
              {t('app.menu')}
            </button>
            <h1 class="min-w-0 truncate text-lg font-semibold">{t('app.title')}</h1>
            <!--
              Pending for C2 (D-M5-01): this control will call POST /api/sessions
              with an empty body and open the returned conversation. It stays
              disabled until that wiring exists. It is the only disabled control
              the shell adds.
            -->
            <button
              type="button"
              class="ui-new-conversation"
              disabled
              title={t('shell.newConversationPending')}
              aria-describedby="new-conversation-pending"
            >
              {t('shell.newConversation')}
            </button>
            <span id="new-conversation-pending" class="ui-visually-hidden">{t('shell.newConversationPending')}</span>
          </div>

          <div class="flex max-w-full flex-wrap items-center gap-2">
            <button
              type="button"
              aria-label={t('app.theme')}
              onclick={toggleTheme}
              class="rounded-lg border border-gray-300 bg-white p-2 text-gray-600 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
            >
              {#if isDark}
                <Sun size={16} />
              {:else}
                <Moon size={16} />
              {/if}
            </button>
            <label class="sr-only" for="app-language-select">{t('app.language')}</label>
            <select
              id="app-language-select"
              bind:value={i18n.lang}
              aria-label={t('app.language')}
              onchange={(event) => setLanguage(event.currentTarget.value)}
              class="max-w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
            >
              {#each languageOptions as option}
                <option value={option.value}>{option.label}</option>
              {/each}
            </select>
            <button
              type="button"
              onclick={logout}
              class="rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm text-gray-700 transition hover:bg-gray-100 dark:border-gray-600 dark:bg-gray-800 dark:text-gray-200 dark:hover:bg-gray-700"
            >
              {t('common.logout')}
            </button>
          </div>
          <ContextHeader status={gatewayStatus} origin={gatewayAddress} />
        </header>

        <main id="console-main" class="min-w-0 max-w-full flex-1 p-4 sm:p-6" tabindex="-1">
          {#if activePath === '/overview'}
            <OverviewPage />
          {:else if activePath === '/decision-plane'}
            <DecisionPlanePage />
          {:else if activePath === '/decisions'}
            <DecisionInspectPage />
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
          {:else if activePath === '/providers'}
            <ProvidersPage />
          {:else if activePath === '/models'}
            <ModelsPage />
          {:else if activePath === '/routing'}
            <RoutingPage />
          {:else if activePath === '/changes'}
            <ChangesPage />
          {:else if activePath === '/config'}
            <ConfigPage />
          {:else if activePath === '/logs'}
            <LogsPage />
          {:else}
            <section class="space-y-4">
              <PageHeader title={t('app.notFound')} />
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
