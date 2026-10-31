<script>
  let { name, label, type = 'text', value = '', error = '', onValue } = $props();

  const fieldId = $derived(`field-${name}`);
  const errorId = $derived(`field-${name}-error`);
</script>

<div class="ui-field">
  <label for={fieldId}>{label}</label>
  <input
    id={fieldId}
    {name}
    {type}
    {value}
    aria-invalid={error ? 'true' : 'false'}
    aria-describedby={error ? errorId : undefined}
    oninput={(event) => {
      const target = event.currentTarget instanceof HTMLInputElement ? event.currentTarget : event.target;
      if (target instanceof HTMLInputElement) {
        onValue?.(target.value);
      }
    }}
    onkeydown={(event) => {
      if (event.key === 'Enter' && (event.isComposing || event.keyCode === 229)) {
        event.preventDefault();
      }
    }}
  />
  {#if error}
    <p id={errorId} class="ui-field-error">{error}</p>
  {/if}
</div>
