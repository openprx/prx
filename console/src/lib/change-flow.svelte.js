import { api } from './api';
import { t } from './i18n';

// Whether a text the gateway sent carries one of its masks: `[redacted]`
// (URL userinfo, a query value or fragment), `[REDACTED]` (a recognised
// secret) or `***` (a secret path). A masked text is display-only: it is
// never prefilled into an editable field and never sent back.
export function isMasked(value) {
  return typeof value === 'string' && (value.toLowerCase().includes('[redacted]') || value.includes('***'));
}

// A loaded text for an editable field: empty when it is masked.
export function unmasked(value) {
  return isMasked(value) ? '' : (value ?? '');
}

// Whether any value of a typed change carries a mask.
export function holdsMask(change) {
  if (typeof change === 'string') return isMasked(change);
  if (!change || typeof change !== 'object') return false;
  return Object.values(change).some(holdsMask);
}

// One risk reason as the plan reports it (`{kind, path?}`).
export function riskReasonText(reason) {
  if (!reason || typeof reason !== 'object') {
    return String(reason ?? '');
  }
  return reason.path ? `${reason.kind}: ${reason.path}` : String(reason.kind ?? '');
}

// The text of the confirmation a 428 asks for: the server's refusal, its
// reasons and the refused plan, so the operator confirms what they saw.
export function confirmationText(error) {
  const lines = [error.message];
  const reasons = Array.isArray(error.body?.risk_reasons) ? error.body.risk_reasons : [];
  for (const reason of reasons) {
    lines.push(`- ${riskReasonText(reason)}`);
  }
  if (typeof error.body?.plan === 'string' && error.body.plan) {
    lines.push('', error.body.plan);
  }
  lines.push('', t('manage.confirmApply'));
  return lines.join('\n');
}

// Send a write bound to `revision` (`If-Match`). A high-risk change answers
// 428 with its plan and the revision that plan was computed against: show
// them, and when the operator agrees resend once with the confirmation,
// bound to exactly that revision (D-049 B). A change landing in between
// answers 409, never an unseen change.
export async function sendConfirming(send, revision, canContinue = () => true) {
  try {
    return await send({ revision, confirmHighRisk: false });
  } catch (e) {
    const refused = e?.body?.revision;
    if (!canContinue() || e?.status !== 428 || !refused || !window.confirm(confirmationText(e)) || !canContinue()) {
      throw e;
    }
    return await send({ revision: refused, confirmHighRisk: true });
  }
}

// Plan, review, apply: a page shows the dry run of a typed change
// (`POST /api/config/plans`), and applies exactly that change with the
// plan's revision (`POST /api/config/apply`).
export class ChangeFlow {
  pending = $state(null);
  busy = $state(false);
  error = $state(null);
  result = $state(null);
  disposed = false;

  constructor(onApplied, transport = { plan: api.planChange, apply: api.applyChange }) {
    this.onApplied = onApplied;
    this.transport = transport;
  }

  async preview(change) {
    if (this.busy || this.disposed) return;
    // A masked value would be refused by the gateway, and must not travel.
    if (holdsMask(change)) {
      this.pending = null;
      this.result = null;
      this.error = new Error(t('manage.maskedValue'));
      return;
    }
    this.busy = true;
    this.error = null;
    this.result = null;
    try {
      const snapshot = JSON.parse(JSON.stringify(change));
      const plan = await this.transport.plan(snapshot);
      if (this.disposed) return;
      this.pending = { change: snapshot, plan };
    } catch (e) {
      if (this.disposed) return;
      this.pending = null;
      this.error = e;
    } finally {
      if (!this.disposed) this.busy = false;
    }
  }

  async apply() {
    const pending = this.pending;
    if (!pending || this.busy || this.disposed) {
      return;
    }
    this.busy = true;
    this.error = null;
    try {
      const result = await sendConfirming(
        (options) => this.transport.apply(pending.change, options),
        pending.plan.revision,
        () => !this.disposed
      );
      if (this.disposed) return;
      this.result = result;
      this.pending = null;
      await this.onApplied();
    } catch (e) {
      if (this.disposed) return;
      this.error = e;
      // A stale plan is useless: the operator plans again on what is there.
      if (e?.status === 409) {
        this.pending = null;
        await this.onApplied();
      }
    } finally {
      if (!this.disposed) this.busy = false;
    }
  }

  // An already-sent transaction may finish, but leaving the page must never
  // trigger a confirmation resend or an update to a replacement page.
  dispose() {
    this.disposed = true;
    this.busy = false;
    this.pending = null;
    this.result = null;
    this.error = null;
  }

  cancel() {
    this.pending = null;
    this.error = null;
  }
}
