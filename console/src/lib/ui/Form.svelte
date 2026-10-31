<script>
  import { untrack } from 'svelte';
  import { t } from '../i18n';
  import Field from './Field.svelte';

  // Submission is ignored while an IME composition is open, so Enter
  // during composition cannot send a partial word. A failed validate()
  // records errors and leaves the current field values in place.
  let { fields = [], initialValues = {}, validate, submitLabel, onValid } = $props();

  let values = $state(untrack(() => ({ ...initialValues })));
  let errors = $state({});
  let composing = $state(false);

  function snapshot() {
    return { ...values };
  }

  function handleSubmit(event) {
    event.preventDefault();
    if (composing || event.isComposing) {
      return;
    }

    const current = snapshot();
    const result = typeof validate === 'function' ? validate(current) : {};
    const nextErrors = {};
    if (result && typeof result === 'object') {
      for (const [key, message] of Object.entries(result)) {
        if (typeof message === 'string' && message.trim().length > 0) {
          nextErrors[key] = message;
        }
      }
    }
    errors = nextErrors;
    if (Object.keys(nextErrors).length > 0) {
      return;
    }
    onValid?.(current);
  }

  function updateValue(name, next) {
    values[name] = next;
  }
</script>

<form
  class="ui-form"
  onsubmit={handleSubmit}
  oncompositionstart={() => (composing = true)}
  oncompositionend={() => (composing = false)}
>
  {#each fields as field (field.name)}
    <Field
      name={field.name}
      label={field.label}
      type={field.type ?? 'text'}
      value={values[field.name] ?? ''}
      error={errors[field.name] ?? ''}
      onValue={(next) => updateValue(field.name, next)}
    />
  {/each}
  {#if Object.keys(errors).length > 0}
    <p class="ui-form-summary" role="alert">{t('form.kept')}</p>
  {/if}
  <button type="submit">{submitLabel}</button>
</form>
