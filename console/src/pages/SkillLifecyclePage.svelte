<script>
  import { api } from '../lib/api';
  import { t } from '../lib/i18n';
  import { operationFailure } from '../lib/operation-state';
  import { navigate } from '../lib/router';
  import DataTable from '../lib/ui/DataTable.svelte';
  import Field from '../lib/ui/Field.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import StateBlock from '../lib/ui/StateBlock.svelte';

  let phase = $state('loading');
  let message = $state('');
  let status = $state(null);
  let proposals = $state([]);
  let selected = $state(null);
  let notice = $state(null);
  let outcome = $state('');
  let reason = $state('');
  let skillId = $state('');
  let revisionId = $state('');
  let generation = $state('');

  function proposalRows(list) {
    return list.map((proposal) => ({
      proposal_id: typeof proposal?.proposal_id === 'string' ? proposal.proposal_id : '',
      skill_id: typeof proposal?.skill_id === 'string' ? proposal.skill_id : '',
      status: typeof proposal?.status === 'string' ? proposal.status : ''
    }));
  }

  function statusRows(value) {
    if (!value || typeof value !== 'object') {
      return [];
    }
    return Object.entries(value)
      .filter(([, entry]) => ['string', 'number', 'boolean'].includes(typeof entry))
      .map(([key, entry]) => ({ field: key, value: String(entry) }));
  }

  function outcomeText(body) {
    if (body?.committed_repair_required === true) {
      return t('operations.lifecycle.repair');
    }
    if (body?.replayed === true) {
      return t('operations.lifecycle.replayed');
    }
    return '';
  }

  $effect(() => {
    let cancelled = false;
    phase = 'loading';
    Promise.all([api.getLifecycleStatus(), api.listLifecycleProposals()])
      .then(([statusBody, listBody]) => {
        if (cancelled) {
          return;
        }
        status = statusBody;
        proposals = Array.isArray(listBody?.proposals) ? listBody.proposals : [];
        phase = 'ready';
      })
      .catch((error) => {
        if (cancelled) {
          return;
        }
        const failure = operationFailure(error);
        phase = failure.variant;
        message = failure.message;
      });
    return () => {
      cancelled = true;
    };
  });

  async function openProposal(id) {
    notice = null;
    outcome = '';
    try {
      selected = await api.getLifecycleProposal(id);
    } catch (error) {
      notice = operationFailure(error);
    }
  }

  async function publish() {
    const id = selected?.proposal_id;
    if (!id) {
      return;
    }
    notice = null;
    outcome = '';
    try {
      const body = await api.publishLifecycleProposal(id);
      outcome = outcomeText(body);
      selected = body;
    } catch (error) {
      const failure = operationFailure(error);
      if (error?.body?.committed_repair_required === true) {
        outcome = t('operations.lifecycle.repair');
      }
      notice = failure;
    }
  }

  async function cancel() {
    const id = selected?.proposal_id;
    if (!id) {
      return;
    }
    notice = null;
    try {
      selected = await api.cancelLifecycleProposal(id, reason.trim());
      outcome = '';
    } catch (error) {
      notice = operationFailure(error);
    }
  }

  async function rollback(event) {
    event.preventDefault();
    notice = null;
    outcome = '';
    const expected = Number(generation);
    if (!skillId.trim() || !revisionId.trim() || !Number.isInteger(expected) || expected < 0) {
      notice = { variant: 'error', message: t('operations.lifecycle.generation') };
      return;
    }
    try {
      const body = await api.rollbackLifecycle({
        skill_id: skillId.trim(),
        revision_id: revisionId.trim(),
        expected_generation: expected
      });
      outcome = outcomeText(body) || (body?.replayed === true ? t('operations.lifecycle.replayed') : '');
    } catch (error) {
      if (error?.body?.committed_repair_required === true) {
        outcome = t('operations.lifecycle.repair');
      }
      notice = operationFailure(error);
    }
  }
</script>

<section class="space-y-4">
  <PageHeader title={t('operations.lifecycle.title')} description={t('operations.lifecycle.description')} />
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.lifecycle.engineering')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.lifecycle.grant')}</p>
  <p class="text-sm text-gray-600 dark:text-gray-300">{t('operations.lifecycle.cli')}</p>
  <button type="button" onclick={() => navigate('/skills')} class="rounded-lg border border-gray-300 px-3 py-2 text-sm">{t('nav.skills')}</button>

  {#if phase === 'loading'}
    <StateBlock variant="loading" message={t('operations.lifecycle.loading')} />
  {:else if phase !== 'ready'}
    <StateBlock variant={phase} {message} />
  {:else}
    <h3 class="text-sm font-semibold">{t('operations.lifecycle.status')}</h3>
    <DataTable
      caption={t('operations.lifecycle.status')}
      columns={[
        { key: 'field', label: t('operations.lifecycle.status') },
        { key: 'value', label: t('operations.lifecycle.status') }
      ]}
      rows={statusRows(status)}
    />
    <h3 class="text-sm font-semibold">{t('operations.lifecycle.proposals')}</h3>
    <DataTable
      caption={t('operations.lifecycle.proposals')}
      columns={[
        { key: 'proposal_id', label: 'proposal_id', truncate: true },
        { key: 'skill_id', label: t('operations.lifecycle.skillId') },
        { key: 'status', label: t('operations.lifecycle.status') }
      ]}
      rows={proposalRows(proposals)}
    />
    <ul class="flex flex-wrap gap-2">
      {#each proposals as proposal (proposal.proposal_id)}
        <li>
          <button type="button" onclick={() => openProposal(proposal.proposal_id)} class="rounded-lg border border-gray-300 px-3 py-2 text-sm">
            {t('operations.lifecycle.open')} {proposal.proposal_id}
          </button>
        </li>
      {/each}
    </ul>
    {#if selected?.proposal_id}
      <article class="space-y-3 rounded-xl border border-gray-200 p-4 dark:border-gray-700">
        <h3 class="break-all font-mono text-sm">{selected.proposal_id}</h3>
        <p>{selected.status ?? t('common.unknown')}</p>
        <button type="button" onclick={publish} class="rounded-lg bg-sky-600 px-3 py-2 text-sm text-white">{t('operations.lifecycle.publish')}</button>
        <Field name="cancel-reason" label={t('operations.lifecycle.reason')} value={reason} onValue={(value) => (reason = value)} />
        <button type="button" onclick={cancel} class="rounded-lg border border-gray-300 px-3 py-2 text-sm">{t('operations.lifecycle.cancel')}</button>
      </article>
    {/if}
    <form class="space-y-3" onsubmit={rollback}>
      <h3 class="text-sm font-semibold">{t('operations.lifecycle.rollback')}</h3>
      <Field name="skill-id" label={t('operations.lifecycle.skillId')} value={skillId} onValue={(value) => (skillId = value)} />
      <Field name="revision-id" label={t('operations.lifecycle.revisionId')} value={revisionId} onValue={(value) => (revisionId = value)} />
      <Field name="generation" label={t('operations.lifecycle.generation')} value={generation} onValue={(value) => (generation = value)} />
      <button type="submit" class="rounded-lg border border-gray-300 px-3 py-2 text-sm">{t('operations.lifecycle.rollback')}</button>
    </form>
  {/if}
  {#if outcome}
    <p role="status">{outcome}</p>
  {/if}
  {#if notice}
    <StateBlock variant={notice.variant} message={notice.message} />
  {/if}
</section>
