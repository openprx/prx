//! Durable prospective liability. This accounting primitive does not prove a
//! transport's charge bound or grant permission to invoke that transport.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[cfg(unix)]
use std::io::Write;
use std::path::PathBuf;

pub(super) const JOURNAL: &str = "costs.reservations.jsonl";

/// A journal write or its post-write state transition was not confirmed.
/// Callers may inspect this typed marker; absence means the rejected operation
/// did not attempt a journal write. Do not release possibly dispatched holds.
#[derive(Debug, thiserror::Error)]
#[error("reservation write was not confirmed: {0}")]
pub(crate) struct ReservationWriteUnconfirmed(#[source] anyhow::Error);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReservationRequest {
    pub turn_id: String,
    pub attempt_id: String,
    /// Digest of the admitted source, provider and priced transport bound.
    pub binding_sha256: String,
    pub maximum_micro_usd: u64,
    pub turn_limit_micro_usd: u64,
    pub security_daily_micro_usd: u64,
}

impl ReservationRequest {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.turn_id.trim().is_empty() && self.turn_id.len() <= 256,
            "invalid reservation turn ID"
        );
        ensure!(
            !self.attempt_id.trim().is_empty() && self.attempt_id.len() <= 256,
            "invalid reservation attempt ID"
        );
        ensure!(
            self.binding_sha256.len() == 64 && self.binding_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid reservation binding digest"
        );
        ensure!(
            self.maximum_micro_usd <= self.turn_limit_micro_usd,
            "attempt exceeds its turn allowance"
        );
        Ok(())
    }
}

/// Versioned estimated liability; legacy ReservationRequest retains its
/// proved-bound journal meaning. This estimate never claims a vendor cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EstimatedReservationRequest {
    pub reservation: ReservationRequest,
    pub reserved_tokens: u64,
    pub turn_token_limit: u64,
}
impl EstimatedReservationRequest {
    fn validate(&self) -> Result<()> {
        self.reservation.validate()?;
        ensure!(
            self.reserved_tokens > 0 && self.reserved_tokens <= self.turn_token_limit,
            "invalid estimated token reservation"
        );
        Ok(())
    }
}
#[derive(Clone)]
struct EstimatedTokens {
    reserved: u64,
    limit: u64,
    actual: Option<u128>,
}

/// Possession is necessary for a transition, but dispatch must still consume
/// the durable Reserved state under the workspace ledger lock.
#[derive(Debug)]
pub(crate) struct ReservationReceipt {
    request: ReservationRequest,
    workspace: PathBuf,
}

impl ReservationReceipt {
    pub(crate) fn attempt_id(&self) -> &str {
        &self.request.attempt_id
    }

    pub(super) const fn security_daily_micro_usd(&self) -> u64 {
        self.request.security_daily_micro_usd
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Reserved,
    Dispatched,
    Released,
    Settled { actual_micro_usd: u128 },
    OverBound { actual_micro_usd: u128 },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    Reserved {
        request: ReservationRequest,
    },
    EstimatedReservedV1 {
        request: EstimatedReservationRequest,
    },
    EstimatedSettledV1 {
        attempt_id: String,
        actual_micro_usd: String,
        actual_tokens: String,
        settlement_sha256: String,
        cost_row_sha256: String,
    },
    Dispatched {
        attempt_id: String,
    },
    Released {
        attempt_id: String,
    },
    Settled {
        attempt_id: String,
        actual_micro_usd: String,
        settlement_sha256: String,
        cost_row_sha256: String,
    },
}

#[derive(Default)]
pub(super) struct Reservations {
    entries: BTreeMap<String, (ReservationRequest, State)>,
    settlements: BTreeMap<String, (String, String)>,
    unreconciled_over_bound: bool,
    estimates: BTreeMap<String, EstimatedTokens>,
}

impl Reservations {
    pub(super) fn load(workspace: &Path) -> Result<Self> {
        let mut result = Self::default();
        if let Some(contents) = super::tracker::read_state_ledger_bounded(workspace, JOURNAL)? {
            for line in contents.lines() {
                // Unlike legacy cost rows, every journal row is a transition.
                let event: Event = serde_json::from_str(line).context("invalid reservation journal row")?;
                result.apply(event)?;
            }
        }
        result.verify_cost_projection(workspace)?;
        Ok(result)
    }

    // A terminal journal transition must never hide a missing or conflicting
    // cost projection after recovery. Read both files under the writer lock;
    // read-only callers still receive only an advisory snapshot.
    fn verify_cost_projection(&mut self, workspace: &Path) -> Result<()> {
        if self.settlements.is_empty() && !self.entries.values().any(|(_, state)| *state == State::Dispatched) {
            return Ok(());
        }
        let Some(ledger) = super::tracker::read_state_ledger_bounded(workspace, "costs.jsonl")? else {
            ensure!(self.settlements.is_empty(), "settled reservation has no cost ledger");
            return Ok(());
        };
        let mut seen = std::collections::BTreeSet::new();
        for line in ledger.lines().filter(|line| !line.trim().is_empty()) {
            let record: super::types::CostRecord = serde_json::from_str(line).context("invalid cost projection row")?;
            let Some(id) = record.settlement_id.as_deref() else {
                continue;
            };
            let Some(expected) = self.settlements.get(id) else {
                if let Some((request, State::Dispatched)) = self.entries.get(id) {
                    // A crash after the cost write must not reopen spending
                    // before the explicit over-bound transition is persisted.
                    if !self.estimates.contains_key(id)
                        && super::tracker::ceil_micro_usd(record.usage.cost_usd)?
                            > u128::from(request.maximum_micro_usd)
                    {
                        self.unreconciled_over_bound = true;
                    }
                }
                continue;
            };
            ensure!(seen.insert(id.to_owned()), "reservation has duplicate cost projections");
            ensure!(
                record.settlement_sha256.as_ref() == Some(&expected.0) && cost_row_sha256(&record)? == expected.1,
                "reservation cost projection digest conflicts"
            );
            if let Some(estimate) = self.estimates.get(id) {
                ensure!(
                    estimate.actual
                        == Some(u128::from(record.usage.input_tokens) + u128::from(record.usage.output_tokens)),
                    "estimated reservation token projection conflicts"
                );
            }
            let (_, state) = self.entries.get(id).context("unknown reservation projection")?;
            let actual = match state {
                State::Settled { actual_micro_usd } | State::OverBound { actual_micro_usd } => *actual_micro_usd,
                _ => anyhow::bail!("reservation projection has no terminal state"),
            };
            ensure!(
                super::tracker::ceil_micro_usd(record.usage.cost_usd)? == actual,
                "reservation cost projection amount conflicts"
            );
        }
        ensure!(
            seen.len() == self.settlements.len(),
            "settled reservation cost projection is missing"
        );
        Ok(())
    }

    fn apply(&mut self, event: Event) -> Result<()> {
        match event {
            Event::EstimatedReservedV1 { request } => {
                request.validate()?;
                self.check_tokens(&request)?;
                let id = request.reservation.attempt_id.clone();
                self.apply(Event::Reserved {
                    request: request.reservation,
                })?;
                self.estimates.insert(
                    id,
                    EstimatedTokens {
                        reserved: request.reserved_tokens,
                        limit: request.turn_token_limit,
                        actual: None,
                    },
                );
            }
            Event::EstimatedSettledV1 {
                attempt_id,
                actual_micro_usd,
                actual_tokens,
                settlement_sha256,
                cost_row_sha256,
            } => {
                let tokens = actual_tokens
                    .parse::<u128>()
                    .context("invalid estimated token settlement")?;
                ensure!(
                    tokens.to_string() == actual_tokens,
                    "noncanonical estimated token settlement"
                );
                ensure!(
                    self.estimates.contains_key(&attempt_id),
                    "estimated settlement has legacy reservation"
                );
                self.apply_settlement(
                    attempt_id.clone(),
                    actual_micro_usd,
                    settlement_sha256,
                    cost_row_sha256,
                    true,
                )?;
                self.estimates
                    .get_mut(&attempt_id)
                    .context("missing estimated reservation")?
                    .actual = Some(tokens);
            }
            Event::Reserved { request } => {
                request.validate()?;
                ensure!(
                    !self.entries.contains_key(&request.attempt_id),
                    "duplicate reservation attempt"
                );
                self.check_turn(&request)?;
                self.entries
                    .insert(request.attempt_id.clone(), (request, State::Reserved));
            }
            Event::Dispatched { attempt_id } => {
                let (_, state) = self
                    .entries
                    .get_mut(&attempt_id)
                    .context("unknown reservation attempt")?;
                ensure!(*state == State::Reserved, "reservation is not available for dispatch");
                *state = State::Dispatched;
            }
            Event::Settled {
                attempt_id,
                actual_micro_usd,
                settlement_sha256,
                cost_row_sha256,
            } => {
                ensure!(
                    !self.estimates.contains_key(&attempt_id),
                    "legacy settlement has estimated reservation"
                );
                self.apply_settlement(attempt_id, actual_micro_usd, settlement_sha256, cost_row_sha256, false)?;
            }
            Event::Released { attempt_id } => {
                let (_, state) = self
                    .entries
                    .get_mut(&attempt_id)
                    .context("unknown reservation attempt")?;
                ensure!(*state == State::Reserved, "only an unsent reservation can be released");
                *state = State::Released;
            }
        }
        Ok(())
    }

    fn apply_settlement(
        &mut self,
        attempt_id: String,
        actual_micro_usd: String,
        settlement_sha256: String,
        cost_row_sha256: String,
        estimated: bool,
    ) -> Result<()> {
        let amount = actual_micro_usd
            .parse::<u128>()
            .context("invalid reservation settlement amount")?;
        ensure!(
            amount.to_string() == actual_micro_usd,
            "noncanonical reservation settlement amount"
        );
        let actual_micro_usd = amount;
        ensure!(
            settlement_sha256.len() == 64 && settlement_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid reservation settlement digest"
        );
        ensure!(
            cost_row_sha256.len() == 64 && cost_row_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid reservation cost row digest"
        );
        let (request, state) = self
            .entries
            .get_mut(&attempt_id)
            .context("unknown settled reservation")?;
        ensure!(
            *state == State::Dispatched,
            "settlement requires a dispatched reservation"
        );
        *state = if estimated || actual_micro_usd <= u128::from(request.maximum_micro_usd) {
            State::Settled { actual_micro_usd }
        } else {
            State::OverBound { actual_micro_usd }
        };
        self.settlements
            .insert(attempt_id, (settlement_sha256, cost_row_sha256));
        Ok(())
    }

    fn check_tokens(&self, request: &EstimatedReservationRequest) -> Result<()> {
        let mut total = u128::from(request.reserved_tokens);
        for (id, (previous, state)) in &self.entries {
            if previous.turn_id != request.reservation.turn_id {
                continue;
            }
            let estimate = self
                .estimates
                .get(id)
                .context("cannot mix legacy and estimated token authority in a turn")?;
            ensure!(
                estimate.limit == request.turn_token_limit,
                "estimated turn token limit changed"
            );
            let liability = if *state == State::Released {
                0
            } else {
                estimate.actual.unwrap_or_else(|| u128::from(estimate.reserved))
            };
            total = total
                .checked_add(liability)
                .context("estimated token liability overflows")?;
        }
        ensure!(
            total <= u128::from(request.turn_token_limit),
            "turn token reservation exhausted"
        );
        Ok(())
    }

    pub(super) fn reserve_estimated(
        &mut self,
        workspace: &Path,
        request: EstimatedReservationRequest,
        remaining: u128,
    ) -> Result<ReservationReceipt> {
        request.validate()?;
        // Any fallible receipt construction must precede the first append;
        // otherwise an unmarked error could follow a durable reservation.
        let canonical_workspace = workspace.canonicalize()?;
        if let Some((previous, state)) = self.entries.get(&request.reservation.attempt_id) {
            let estimate = self
                .estimates
                .get(&request.reservation.attempt_id)
                .context("estimated reservation conflicts with legacy")?;
            ensure!(
                previous == &request.reservation
                    && *state == State::Reserved
                    && estimate.reserved == request.reserved_tokens
                    && estimate.limit == request.turn_token_limit,
                "estimated reservation replay conflicts or consumed"
            );
        } else {
            self.check_turn(&request.reservation)?;
            self.check_tokens(&request)?;
            ensure!(
                u128::from(request.reservation.maximum_micro_usd) <= remaining,
                "workspace reservation allowance exhausted"
            );
            let event = Event::EstimatedReservedV1 {
                request: request.clone(),
            };
            append(workspace, &event)?;
            self.apply(event).map_err(ReservationWriteUnconfirmed)?;
        }
        Ok(ReservationReceipt {
            request: request.reservation,
            workspace: canonical_workspace,
        })
    }

    pub(super) fn settle_estimated(
        &mut self,
        workspace: &Path,
        receipt: &ReservationReceipt,
        actual_micro_usd: u128,
        actual_tokens: u128,
        settlement_sha256: String,
        cost_row_sha256: String,
    ) -> Result<()> {
        ensure!(
            workspace.canonicalize()? == receipt.workspace,
            "estimated receipt belongs to another workspace"
        );
        let (previous, state) = self
            .entries
            .get(&receipt.request.attempt_id)
            .context("unknown estimated receipt")?;
        let estimate = self
            .estimates
            .get(&receipt.request.attempt_id)
            .context("estimated receipt has legacy reservation")?;
        ensure!(previous == &receipt.request, "estimated receipt conflicts");
        if let State::Settled {
            actual_micro_usd: prior,
        } = state
        {
            ensure!(
                *prior == actual_micro_usd
                    && estimate.actual == Some(actual_tokens)
                    && self
                        .settlements
                        .get(&receipt.request.attempt_id)
                        .is_some_and(|(s, r)| s == &settlement_sha256 && r == &cost_row_sha256),
                "estimated settlement replay conflicts"
            );
            return Ok(());
        }
        ensure!(*state == State::Dispatched, "estimated receipt was not dispatched");
        let event = Event::EstimatedSettledV1 {
            attempt_id: receipt.request.attempt_id.clone(),
            actual_micro_usd: actual_micro_usd.to_string(),
            actual_tokens: actual_tokens.to_string(),
            settlement_sha256,
            cost_row_sha256,
        };
        append(workspace, &event)?;
        self.apply(event)
            .map_err(|error| ReservationWriteUnconfirmed(error).into())
    }

    fn check_turn(&self, request: &ReservationRequest) -> Result<()> {
        let mut total = u128::from(request.maximum_micro_usd);
        for (previous, state) in self
            .entries
            .values()
            .filter(|(entry, _)| entry.turn_id == request.turn_id)
        {
            ensure!(
                previous.turn_limit_micro_usd == request.turn_limit_micro_usd,
                "reservation turn allowance changed"
            );
            ensure!(
                previous.security_daily_micro_usd == request.security_daily_micro_usd,
                "reservation turn security ceiling changed"
            );
            let liability = match state {
                State::Released => 0,
                State::Settled { actual_micro_usd } => *actual_micro_usd,
                State::OverBound { .. } => anyhow::bail!("reservation transport exceeded its proven bound"),
                State::Reserved | State::Dispatched => u128::from(previous.maximum_micro_usd),
            };
            total = total.checked_add(liability).context("turn reservation sum overflows")?;
        }
        ensure!(
            total <= u128::from(request.turn_limit_micro_usd),
            "turn reservation allowance exhausted"
        );
        Ok(())
    }

    pub(super) fn active_micro_usd(&self) -> Result<u128> {
        ensure!(
            !self.unreconciled_over_bound,
            "unreconciled reservation exceeded its proven bound"
        );
        ensure!(
            !self
                .entries
                .values()
                .any(|(_, state)| matches!(state, State::OverBound { .. })),
            "reservation transport exceeded its proven bound"
        );
        self.entries
            .values()
            .filter(|(_, state)| matches!(state, State::Reserved | State::Dispatched))
            .try_fold(0_u128, |sum, (request, _)| {
                sum.checked_add(u128::from(request.maximum_micro_usd))
                    .context("workspace reservation sum overflows")
            })
    }

    /// Caller owns the same cross-process lock as cost settlement.
    #[cfg(test)]
    pub(super) fn reserve(
        &mut self,
        workspace: &Path,
        request: ReservationRequest,
        remaining: u128,
    ) -> Result<ReservationReceipt> {
        request.validate()?;
        if let Some((previous, state)) = self.entries.get(&request.attempt_id) {
            ensure!(
                previous == &request && *state == State::Reserved,
                "reservation replay conflicts or was consumed"
            );
            return Ok(ReservationReceipt {
                request,
                workspace: workspace.canonicalize()?,
            });
        }
        self.check_turn(&request)?;
        ensure!(
            u128::from(request.maximum_micro_usd) <= remaining,
            "workspace reservation allowance exhausted"
        );
        let event = Event::Reserved {
            request: request.clone(),
        };
        append(workspace, &event)?;
        self.apply(event).map_err(ReservationWriteUnconfirmed)?;
        Ok(ReservationReceipt {
            request,
            workspace: workspace.canonicalize()?,
        })
    }

    #[cfg(test)]
    pub(super) fn recover(&self, workspace: &Path, request: ReservationRequest) -> Result<ReservationReceipt> {
        let (previous, state) = self
            .entries
            .get(&request.attempt_id)
            .context("unknown reservation recovery")?;
        ensure!(
            previous == &request
                && matches!(
                    state,
                    State::Dispatched | State::Settled { .. } | State::OverBound { .. }
                ),
            "reservation recovery conflicts or was not dispatched"
        );
        Ok(ReservationReceipt {
            request,
            workspace: workspace.canonicalize()?,
        })
    }

    /// Called only after the exact canonical cost projection was durably
    /// written and verified under the same ledger lock.
    #[cfg(test)]
    pub(super) fn settle(
        &mut self,
        workspace: &Path,
        receipt: &ReservationReceipt,
        actual_micro_usd: u128,
        settlement_sha256: String,
        cost_row_sha256: String,
    ) -> Result<()> {
        ensure!(
            workspace.canonicalize()? == receipt.workspace,
            "reservation receipt belongs to another workspace"
        );
        let (previous, state) = self
            .entries
            .get(&receipt.request.attempt_id)
            .context("unknown settlement receipt")?;
        ensure!(previous == &receipt.request, "reservation settlement receipt conflicts");
        match state {
            State::Dispatched => {}
            State::Settled {
                actual_micro_usd: prior,
            }
            | State::OverBound {
                actual_micro_usd: prior,
            } => {
                ensure!(
                    *prior == actual_micro_usd
                        && self.settlements.get(&receipt.request.attempt_id).is_some_and(
                            |(settlement, row)| settlement == &settlement_sha256 && row == &cost_row_sha256
                        ),
                    "reservation settlement replay conflicts"
                );
                ensure!(
                    !matches!(state, State::OverBound { .. }),
                    "reservation transport exceeded its proven bound"
                );
                return Ok(());
            }
            _ => anyhow::bail!("reservation was not dispatched"),
        }
        let exceeded = actual_micro_usd > u128::from(previous.maximum_micro_usd);
        let event = Event::Settled {
            attempt_id: receipt.request.attempt_id.clone(),
            // Decimal text preserves the full integer range through tagged
            // JSON enum decoding and consumers without 128-bit JSON numbers.
            actual_micro_usd: actual_micro_usd.to_string(),
            settlement_sha256,
            cost_row_sha256,
        };
        append(workspace, &event)?;
        self.apply(event).map_err(ReservationWriteUnconfirmed)?;
        ensure!(!exceeded, "reservation transport exceeded its proven bound");
        Ok(())
    }

    pub(super) fn transition(&mut self, workspace: &Path, receipt: &ReservationReceipt, dispatch: bool) -> Result<()> {
        ensure!(
            workspace.canonicalize()? == receipt.workspace,
            "reservation receipt belongs to another workspace"
        );
        let (previous, state) = self
            .entries
            .get(&receipt.request.attempt_id)
            .context("unknown reservation receipt")?;
        ensure!(
            previous == &receipt.request && *state == State::Reserved,
            "reservation receipt conflicts or was consumed"
        );
        let event = if dispatch {
            Event::Dispatched {
                attempt_id: receipt.request.attempt_id.clone(),
            }
        } else {
            Event::Released {
                attempt_id: receipt.request.attempt_id.clone(),
            }
        };
        append(workspace, &event)?;
        self.apply(event)
            .map_err(|error| ReservationWriteUnconfirmed(error).into())
    }
}

/// Bind the entire persisted projection, including period attribution and
/// token/model identity, rather than trusting its stored settlement digest.
pub(super) fn cost_row_sha256(record: &super::types::CostRecord) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"prx-cost-reservation-projection-v1\0");
    hash.update(serde_json::to_vec(record)?);
    Ok(hex::encode(hash.finalize()))
}

#[cfg(unix)]
fn append_inner(workspace: &Path, event: &Event) -> Result<()> {
    use rustix::fs::{FileType, Mode, OFlags};
    let directory = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root = rustix::fs::open(workspace, directory, Mode::empty())?;
    let state = rustix::fs::openat(&root, "state", directory, Mode::empty())?;
    ensure!(
        rustix::fs::fstat(&state)?.st_mode & 0o022 == 0,
        "reservation directory is writable by another account"
    );
    let fd = rustix::fs::openat(
        &state,
        JOURNAL,
        OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let stat = rustix::fs::fstat(&fd)?;
    ensure!(
        FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
            && stat.st_nlink == 1
            && stat.st_mode & 0o022 == 0,
        "reservation journal is not a private single-link regular file"
    );
    let mut encoded = serde_json::to_vec(event)?;
    encoded.push(b'\n');
    ensure!(
        stat.st_size >= 0 && (stat.st_size as u64).saturating_add(encoded.len() as u64) <= 8 * 1024 * 1024,
        "reservation journal exceeds its bound"
    );
    let mut file = std::fs::File::from(fd);
    file.write_all(&encoded)?;
    file.sync_all()?;
    // Persist directory creation of the journal before returning authority.
    std::fs::File::from(state).sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn append_inner(_workspace: &Path, _event: &Event) -> Result<()> {
    anyhow::bail!("safe reservation persistence requires Unix file handles")
}

fn append(workspace: &Path, event: &Event) -> Result<()> {
    append_inner(workspace, event).map_err(|error| ReservationWriteUnconfirmed(error).into())
}
