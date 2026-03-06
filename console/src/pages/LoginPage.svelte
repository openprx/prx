<script>
  import { setToken } from '../lib/auth';
  import { i18n, t, toggleLanguage } from '../lib/i18n';

  let { onLogin } = $props();

  let tokenInput = $state('');
  let errorMessage = $state('');

  function submit(event) {
    event.preventDefault();

    const token = tokenInput.trim();
    if (!token) {
      errorMessage = t('login.tokenRequired');
      return;
    }

    setToken(token);
    errorMessage = '';
    onLogin?.(token);
  }
</script>

<div class="flex min-h-screen items-center justify-center bg-gray-900 px-4 py-8 text-gray-100">
  <div class="w-full max-w-md rounded-xl border border-gray-700 bg-gray-800 p-6 shadow-xl shadow-black/30">
    <div class="flex items-center justify-between gap-3">
      <h1 class="text-2xl font-semibold tracking-tight">{t('login.title')}</h1>
      <button
        type="button"
        aria-label={t('app.language')}
        onclick={toggleLanguage}
        class="rounded-lg border border-gray-600 bg-gray-900 px-3 py-2 text-sm text-gray-200 transition hover:bg-gray-700"
      >
        {i18n.lang === 'zh' ? '中文 / EN' : 'EN / 中文'}
      </button>
    </div>
    <p class="mt-2 text-sm text-gray-400">{t('login.hint')}</p>

    <form class="mt-6 space-y-4" onsubmit={submit}>
      <label class="block text-sm font-medium text-gray-300" for="token">{t('login.accessToken')}</label>
      <input
        id="token"
        type="password"
        bind:value={tokenInput}
        class="w-full rounded-lg border border-gray-600 bg-gray-900 px-3 py-2 text-gray-100 outline-none ring-sky-500 transition focus:ring-2"
        placeholder={t('login.placeholder')}
        autocomplete="off"
      />

      {#if errorMessage}
        <p class="text-sm text-red-400">{errorMessage}</p>
      {/if}

      <button
        type="submit"
        class="w-full rounded-lg bg-sky-600 px-4 py-2 font-medium text-white transition hover:bg-sky-500"
      >
        {t('login.login')}
      </button>
    </form>
  </div>
</div>
