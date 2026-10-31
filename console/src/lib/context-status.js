import { t } from './i18n';

const RELOAD_STATUS_KEYS = {
  idle: 'shell.reloadIdle',
  failed: 'shell.reloadFailed',
  unchanged: 'shell.reloadUnchanged',
  applied: 'shell.reloadApplied',
  applied_with_restart_required: 'shell.reloadAppliedRestart',
  restart_required: 'shell.reloadRestartRequired'
};

function unknown() {
  return t('common.unknown');
}

function filled(value) {
  return typeof value === 'string' && value.trim().length > 0 ? value : unknown();
}

function textValue(value) {
  if (typeof value !== 'string') {
    return unknown();
  }
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : unknown();
}

export function gatewayOrigin(configured) {
  if (typeof configured === 'string' && configured.trim().length > 0) {
    return configured.trim().replace(/\/+$/, '');
  }

  if (typeof window !== 'undefined' && typeof window.location?.origin === 'string') {
    const origin = window.location.origin.trim();
    if (origin.length > 0 && origin !== 'null') {
      return origin;
    }
  }

  return '';
}

function generationValue(status) {
  if (!status || typeof status.active_config_generation !== 'number') {
    return unknown();
  }
  if (!Number.isFinite(status.active_config_generation)) {
    return unknown();
  }
  return String(status.active_config_generation);
}

function reloadValue(status) {
  if (!status) {
    return unknown();
  }
  if (status.config_reload_in_progress === true) {
    return t('shell.reloadInProgress');
  }
  if (typeof status.config_reload_status !== 'string') {
    return unknown();
  }
  const key = RELOAD_STATUS_KEYS[status.config_reload_status.trim()];
  return key ? t(key) : unknown();
}

function restartValue(status) {
  if (!status || !Array.isArray(status.restart_required)) {
    return unknown();
  }
  if (status.restart_required.length === 0) {
    return t('shell.restartNotNeeded');
  }
  const names = status.restart_required
    .filter((item) => typeof item === 'string' && item.trim().length > 0)
    .map((item) => item.trim());
  if (names.length === 0) {
    return unknown();
  }
  return `${t('shell.restartNeeded')}: ${names.join(', ')}`;
}

function degradationValue(status) {
  if (!status || typeof status.provider_degraded !== 'boolean') {
    return unknown();
  }
  return status.provider_degraded ? t('shell.degraded') : t('shell.notDegraded');
}

export function presentContext(status, origin) {
  const record = status && typeof status === 'object' ? status : null;
  const facts = [
    { id: 'origin', label: t('shell.origin'), value: textValue(origin) },
    { id: 'version', label: t('overview.version'), value: textValue(record?.version) },
    { id: 'generation', label: t('shell.generation'), value: generationValue(record) },
    {
      id: 'activeRevision',
      label: t('shell.activeRevision'),
      value: textValue(record?.active_config_source_revision)
    },
    {
      id: 'desiredRevision',
      label: t('shell.desiredRevision'),
      value: textValue(record?.desired_config_source_revision)
    },
    { id: 'reload', label: t('shell.reload'), value: reloadValue(record) },
    { id: 'restart', label: t('shell.restartRequired'), value: restartValue(record) },
    { id: 'degradation', label: t('shell.providerDegradation'), value: degradationValue(record) }
  ];

  return facts.map((fact) => ({
    id: fact.id,
    label: fact.label,
    value: filled(fact.value)
  }));
}
