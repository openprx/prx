use super::{
    AssignmentResult, ChatSessionError, ChatSessions, MAX_SUMMARY_LEN, PublicationTurn, RESULT_LOG_CAPACITY,
    ResultStatus, ResultsPage, SESSIONS, authenticate, bounded,
};

// ── Reporting back ──────────────────────────────────────────────────────────

/// Record the outcome of an assignment and retire its work-registry row.
///
/// Accepted from either the delivered or the accepted set, so a session that
/// finishes a task quickly and reports it before its next pull is not punished
/// for skipping the acknowledgement.
///
/// # Errors
///
/// [`ChatSessionError::UnknownSession`], [`ChatSessionError::BadToken`],
/// [`ChatSessionError::UnknownAssignment`], or [`ChatSessionError::Invalid`]
/// when the summary is empty or too long.
pub fn report(
    session_id: &str,
    token: &str,
    assignment_id: &str,
    status: ResultStatus,
    summary: &str,
) -> Result<u64, ChatSessionError> {
    SESSIONS.report(session_id, token, assignment_id, status, summary)
}

/// Results with a sequence greater than `after_seq`, oldest first.
#[must_use]
pub fn results_after(after_seq: u64, limit: usize) -> ResultsPage {
    SESSIONS.results_after(after_seq, limit)
}

impl ChatSessions {
    pub(super) fn report(
        &self,
        session_id: &str,
        token: &str,
        assignment_id: &str,
        status: ResultStatus,
        summary: &str,
    ) -> Result<u64, ChatSessionError> {
        let session = self.find(session_id).ok_or(ChatSessionError::UnknownSession)?;
        authenticate(&session, token)?;
        let summary = bounded(summary, "summary", MAX_SUMMARY_LEN)?;
        let assignment_id = assignment_id.trim();

        // Taken before the assignment leaves its queue, not after: a retry of
        // this same report that finds the queue empty then waits here until
        // the original is on the feed, and is answered with its sequence
        // rather than told the assignment is unknown.
        let turn = PublicationTurn::take(self);
        let assignment = {
            let mut state = session.state.lock();
            state
                .accepted
                .iter()
                .position(|entry| entry.id == assignment_id)
                .map(|index| state.accepted.remove(index))
                .or_else(|| {
                    state
                        .delivered
                        .iter()
                        .position(|entry| entry.id == assignment_id)
                        .map(|index| state.delivered.remove(index))
                })
        };

        let Some(assignment) = assignment else {
            // Not outstanding. Three different things look identical from here,
            // and only the recorded result tells them apart:
            //
            // - this *is* the report, arriving a second time because the first
            //   answer was lost in flight. A chat that is told to retry a report
            //   it already made must not be told the work is unknown, and must
            //   not mint a second completion sequence for it, so the recorded
            //   result answers the replay;
            // - the assignment was ended from outside while the session was
            //   still working on it. The record under this id is then a
            //   cancellation this session never authored, and answering the
            //   report with its sequence would call an operator's kill a
            //   successful report and drop the session's own status and summary
            //   on the floor;
            // - this session never held it, which is the refusal it has always
            //   been.
            let answer = match self.recorded_result(&session.id, assignment_id) {
                Some((_, ResultStatus::Cancelled, summary)) => Err(ChatSessionError::AssignmentCancelled { summary }),
                Some((seq, _, _)) => Ok(seq),
                None => Err(ChatSessionError::UnknownAssignment),
            };
            drop(turn);
            return answer;
        };

        // Retirement and result are one commit. Written separately, a failure
        // between them takes the work out of every queue while leaving no
        // record of it, and the reporter cannot recover by retrying because the
        // assignment is no longer anywhere to retry against.
        let seq = {
            let (record, minted) = turn.mint(&session, &assignment, status, summary);
            turn.write("complete_assignment", |store| {
                store.complete_assignment(&assignment.id, &record)
            });
            turn.publish(minted)
        };
        drop(turn);
        Ok(seq)
    }

    /// The result already recorded for this assignment: its sequence, what it
    /// says, and why.
    ///
    /// The status is what distinguishes a replay of the session's own report
    /// from a cancellation recorded against the same pair of ids by an operator
    /// kill or a deregistration cascade — [`ResultStatus::Cancelled`] is this
    /// runtime's own verdict and [`ResultStatus::parse`] refuses it on the
    /// wire, so no session can ever have authored one.
    ///
    /// Bounded by the feed's retention: a replay that arrives after the record
    /// has been trimmed is still refused as unknown, which is honest — the
    /// answer it is asking for no longer exists to be handed back.
    fn recorded_result(&self, session_id: &str, assignment_id: &str) -> Option<(u64, ResultStatus, String)> {
        let log = self.results.lock();
        log.entries
            .iter()
            .rev()
            .find(|entry| entry.assignment_id == assignment_id && entry.session_id == session_id)
            .map(|entry| (entry.seq, entry.status, entry.summary.clone()))
    }

    pub(super) fn results_after(&self, after_seq: u64, limit: usize) -> ResultsPage {
        let log = self.results.lock();
        let limit = limit.clamp(1, RESULT_LOG_CAPACITY);
        let results = log
            .entries
            .iter()
            .filter(|entry| entry.seq > after_seq)
            .take(limit)
            .map(|entry| AssignmentResult {
                seq: entry.seq,
                assignment_id: entry.assignment_id.clone(),
                session_id: entry.session_id.clone(),
                session_label: entry.session_label.clone(),
                disposition: entry.disposition,
                status: entry.status,
                summary: entry.summary.clone(),
                completed_at_unix_ms: entry.completed_at_unix_ms,
                origin: entry.origin.clone(),
            })
            .collect::<Vec<_>>();
        let next_seq = results.last().map_or(after_seq, |entry| entry.seq);
        ResultsPage {
            results,
            next_seq,
            oldest_seq: log.oldest_seq,
        }
    }
}
