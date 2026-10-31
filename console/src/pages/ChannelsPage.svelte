<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { channelSendOutcome, loadFailureVariant } from '../lib/load-state';
  import Field from '../lib/ui/Field.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  let channels = $state([]);
  let loading = $state(true);
  let loadError = $state(null);
  let lastUpdated = $state('');

  let channelName = $state('');
  let recipient = $state('');
  let message = $state('');
  let asVoice = $state(false);
  let sending = $state(false);
  let sendOutcome = $state('');
  let sendError = $state('');
  let sendResult = $state(null);

  function humanizeKey(value) {
    if (typeof value !== 'string' || value.length === 0) {
      return t('common.unknown');
    }

    return value
      .replaceAll('_', ' ')
      .split(' ')
      .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
      .join(' ');
  }

  function channelLabel(name) {
    const key = `channels.names.${name}`;
    const translated = t(key);
    return translated === key ? humanizeKey(name) : translated;
  }

  function shownField(payload, key) {
    if (!payload || typeof payload !== 'object' || !Object.hasOwn(payload, key)) {
      return t('channels.resultMissing');
    }
    const value = payload[key];
    if (typeof value === 'boolean') {
      return value ? 'true' : 'false';
    }
    if (value === null || value === undefined) {
      return 'null';
    }
    return String(value);
  }

  async function loadChannels() {
    try {
      const response = await api.getChannelsStatus();
      channels = Array.isArray(response?.channels) ? response.channels : [];
      loadError = null;
      lastUpdated = new Date().toLocaleTimeString();
      if (!channelName && channels.length > 0 && typeof channels[0]?.name === 'string') {
        channelName = channels[0].name;
      }
    } catch (error) {
      loadError = error;
    } finally {
      loading = false;
    }
  }

  async function submitSend(event) {
    event.preventDefault();
    if (sending) return;
    sending = true;
    sendOutcome = '';
    sendError = '';
    sendResult = null;
    try {
      const response = await api.sendChannel(channelName, { recipient, message, asVoice });
      sendResult = response && typeof response === 'object' ? response : {};
      sendOutcome = 'sent';
    } catch (error) {
      sendOutcome = channelSendOutcome(error);
      if (sendOutcome !== 'uncertain') {
        sendError = error instanceof Error ? error.message : t('channels.denied');
      }
    } finally {
      sending = false;
    }
  }

  $effect(() => {
    let cancelled = false;

    const refresh = async () => {
      if (!cancelled) {
        await loadChannels();
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

<section class="space-y-6">
  <div class="flex items-center justify-between">
    <PageHeader title={t('channels.title')} />
    {#if lastUpdated}
      <span class="text-xs text-gray-500 dark:text-gray-400">{t('common.updatedAt', { time: lastUpdated })}</span>
    {/if}
  </div>

  {#if loading}
    <StateBlock variant="loading" message={t('channels.loading')} />
  {:else if loadError}
    <StateBlock variant={loadFailureVariant(loadError)} message={loadError?.message || t('channels.loadFailed')} />
  {:else}
    <p class="text-sm text-gray-600 dark:text-gray-300">{t('channels.statusHint')}</p>
    {#if channels.length === 0}
      <StateBlock variant="empty" message={t('channels.noChannels')} />
    {:else}
      <div class="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {#each channels as channel}
          <article class="rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800">
            <div class="flex items-start justify-between gap-3">
              <h3 class="text-lg font-semibold text-gray-900 dark:text-gray-100">{channelLabel(channel.name)}</h3>
              <span
                class={`rounded-full px-2 py-1 text-xs font-medium ${
                  channel.enabled
                    ? 'border border-green-500/50 bg-green-500/20 text-green-700 dark:text-green-300'
                    : 'border border-red-500/50 bg-red-500/20 text-red-700 dark:text-red-300'
                }`}
              >
                {channel.enabled ? t('common.enabled') : t('common.disabled')}
              </span>
            </div>
            <p class="mt-3 text-sm text-gray-500 dark:text-gray-400">{t('channels.type')}: {channelLabel(channel.type)}</p>
            <p class="mt-1 text-sm text-gray-500 dark:text-gray-400">
              {t('channels.configuredLabel')}: {channelLabel(channel.status)}
            </p>
          </article>
        {/each}
      </div>
    {/if}

    <form class="space-y-3 rounded-xl border border-gray-200 bg-white p-4 dark:border-gray-700 dark:bg-gray-800" onsubmit={submitSend}>
      <h3 class="text-base font-semibold text-gray-900 dark:text-gray-100">{t('channels.sendTitle')}</h3>
      <Field name="channel" label={t('channels.title')} value={channelName} onValue={(value) => (channelName = value)} />
      <Field name="recipient" label={t('channels.recipient')} value={recipient} onValue={(value) => (recipient = value)} />
      <div class="ui-field">
        <label for="channel-message">{t('channels.message')}</label>
        <textarea
          id="channel-message"
          name="message"
          rows="3"
          bind:value={message}
          class="w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm dark:border-gray-600 dark:bg-gray-900"
        ></textarea>
      </div>
      <label class="flex items-center gap-2 text-sm text-gray-700 dark:text-gray-200">
        <input type="checkbox" name="as_voice" bind:checked={asVoice} />
        {t('channels.asVoice')}
      </label>
      <button
        type="submit"
        disabled={sending || channelName.trim().length === 0}
        class="rounded-lg bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
      >
        {sending ? t('channels.sending') : t('channels.send')}
      </button>
      {#if sendOutcome === 'denied'}
        <StateBlock variant="denied" message={sendError || t('channels.denied')} />
      {:else if sendOutcome === 'uncertain'}
        <StateBlock variant="unavailable" message={t('channels.uncertain')} />
      {:else if sendOutcome === 'error'}
        <StateBlock variant="error" message={sendError} />
      {:else if sendOutcome === 'sent'}
        <dl class="grid gap-1 text-sm sm:grid-cols-2">
          <dt class="text-gray-500 dark:text-gray-400">{t('channels.resultChannel')}</dt>
          <dd>{shownField(sendResult, 'channel')}</dd>
          <dt class="text-gray-500 dark:text-gray-400">{t('channels.resultDelivered')}</dt>
          <dd>{shownField(sendResult, 'delivered')}</dd>
          <dt class="text-gray-500 dark:text-gray-400">{t('channels.resultDetail')}</dt>
          <dd>{shownField(sendResult, 'detail')}</dd>
        </dl>
      {/if}
    </form>
  {/if}
</section>
