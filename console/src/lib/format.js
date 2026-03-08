import { t } from './i18n';

export function formatUptime(totalSeconds) {
  if (!Number.isFinite(totalSeconds) || totalSeconds < 0) {
    return `0${t('common.durationUnits.second')}`;
  }

  const days = Math.floor(totalSeconds / 86400);
  const hours = Math.floor((totalSeconds % 86400) / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = Math.floor(totalSeconds % 60);

  const parts = [];
  if (days > 0) {
    parts.push(`${days}${t('common.durationUnits.day')}`);
  }
  if (hours > 0 || parts.length > 0) {
    parts.push(`${hours}${t('common.durationUnits.hour')}`);
  }
  if (minutes > 0 || parts.length > 0) {
    parts.push(`${minutes}${t('common.durationUnits.minute')}`);
  }
  parts.push(`${seconds}${t('common.durationUnits.second')}`);

  return parts.join(' ');
}

export function formatDateTime(value) {
  if (!value) {
    return '';
  }

  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) {
    return String(value);
  }

  return parsed.toLocaleString();
}
