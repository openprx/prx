<script>
  import { presentContext } from '../context-status';
  import { t } from '../i18n';
  import TruncatedValue from './TruncatedValue.svelte';

  let { status = null, origin = '' } = $props();
  const facts = $derived(presentContext(status, origin));
</script>

<section class="ui-context w-full min-w-0 basis-full" aria-label={t('shell.contextLabel')}>
  <dl>
    {#each facts as fact (fact.id)}
      <div>
        <dt>{fact.label}</dt>
        <dd><TruncatedValue value={fact.value} /></dd>
      </div>
    {/each}
  </dl>
</section>
