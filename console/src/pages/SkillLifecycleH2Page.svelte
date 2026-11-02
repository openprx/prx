<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { operationFailure } from '../lib/operation-state';
  import { navigate } from '../lib/router';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  const ACTIONS = [
    'lifecycle_experiment_catalog',
    'lifecycle_experiment_plan',
    'lifecycle_experiment_admit',
    'lifecycle_experiment_run',
    'lifecycle_experiment_inspect',
    'lifecycle_experiment_cancel',
    'lifecycle_experiment_recover',
    'lifecycle_experiment_review',
    'lifecycle_experiment_publish',
    'lifecycle_experiment_annotate',
    'lifecycle_experiment_rollback',
    'lifecycle_experiment_retire',
    'lifecycle_experiment_lessons'
  ];

  let action = $state(ACTIONS[0]);
  let requestText = $state('{}');
  let phase = $state('ready');
  let message = $state('');
  let invalid = $state(false);

  async function submit(event) {
    event.preventDefault();
    invalid = false;
    message = '';
    phase = 'ready';
    let parsed;
    try {
      parsed = JSON.parse(requestText);
    } catch {
      invalid = true;
      return;
    }
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      invalid = true;
      return;
    }
    try {
      await api.postLifecycleExperiment(action, parsed);
    } catch (error) {
      const failure = operationFailure(error);
      phase = failure.variant;
      message = failure.message;
    }
  }
</script>

<section class="space-y-4">
  <PageHeader title={t('operations.h2.title')} description={t('operations.h2.description')} />
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.h2.engineering')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.h2.mintGap')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.h2.controlled')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.h2.noJob')}</p>
  <button type="button" onclick={() => navigate('/skills')} class="rounded-lg border border-gray-300 px-3 py-2 text-sm">{t('nav.skills')}</button>
  <form class="space-y-3" onsubmit={submit}>
    <div>
      <label for="h2-action">{t('operations.h2.action')}</label>
      <select id="h2-action" bind:value={action} class="mt-1 block w-full max-w-xl rounded border border-gray-400 bg-transparent p-2">
        {#each ACTIONS as name}
          <option value={name}>{name}</option>
        {/each}
      </select>
    </div>
    <div>
      <label for="h2-request">{t('operations.h2.request')}</label>
      <textarea id="h2-request" bind:value={requestText} rows="6" class="mt-1 block w-full rounded border border-gray-400 bg-transparent p-2 font-mono text-xs"></textarea>
    </div>
    {#if invalid}
      <p role="alert">{t('operations.h2.invalidJson')}</p>
    {/if}
    <button type="submit" class="rounded-lg bg-sky-600 px-3 py-2 text-sm text-white">{t('operations.h2.submit')}</button>
  </form>
  {#if phase !== 'ready'}
    <StateBlock variant={phase} {message} />
  {/if}
</section>
