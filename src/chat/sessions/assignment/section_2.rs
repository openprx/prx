#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn assignment(id: &str, task: &str, disposition: &str) -> PulledAssignment {
        PulledAssignment {
            assignment_id: id.to_string(),
            session_id: "session".to_string(),
            task: task.to_string(),
            disposition: disposition.to_string(),
            deliveries: 1,
            created_at_unix_ms: 0,
            work_id: "w1".to_string(),
            origin_channel: "wacli".to_string(),
            origin_ref: Some("3d1a4f5b".to_string()),
        }
    }

    fn silent() -> Arc<dyn Fn(&str) + Send + Sync> {
        Arc::new(|_: &str| {})
    }

    fn client_for_tests() -> Arc<AssignmentClient> {
        let config = Config::default();
        AssignmentClient::new(&config)
    }

    /// One scripted answer to a pull.
    enum Answer {
        /// Hand these assignments over.
        Batch(Vec<PulledAssignment>),
        /// Refuse with this reason.
        Refuse(&'static str),
        /// Refuse the way a daemon that has forgotten this session does: a
        /// `404` carrying the registry's own machine tag.
        ForgotTheSession,
        /// Refuse the way a daemon answers when the *assignment* is unknown —
        /// the same status, a different tag, and never a reason to register
        /// again.
        ForgotTheAssignment,
    }

    /// A refusal in the shape the daemon really sends one, so the poller's
    /// decision is made on the wire form rather than on a sentence.
    fn refusal(status: u16, code: &str, detail: &str) -> anyhow::Error {
        let body = serde_json::json!({ "error": detail, "code": code }).to_string();
        tasks_cli::refusal_for_tests(status, &body, "chat assignment pull")
    }

    /// Records every pull's `ack` array and every report, and answers from a
    /// script. Enough to assert the loop's ordering without a live daemon.
    struct RecordingMailbox {
        base_url: String,
        pulls: Mutex<Vec<Vec<String>>>,
        reports: Mutex<Vec<(String, ResultStatus, String)>>,
        script: Mutex<Vec<Answer>>,
        taken: Mutex<HashSet<String>>,
        seq: AtomicUsize,
        /// Which enrolment this double is on; bumped by every registration.
        generation: Mutex<u64>,
        /// How many times the poller registered again.
        registrations: AtomicUsize,
        /// How many times it *tried*, successful or not: what the re-enrolment
        /// rate limit is about.
        re_enrol_attempts: AtomicUsize,
        /// Makes registration fail, for the case where the daemon is not just
        /// forgetful but down.
        refuse_registration: std::sync::atomic::AtomicBool,
    }

    impl RecordingMailbox {
        fn new(script: Vec<Vec<PulledAssignment>>) -> Arc<Self> {
            Self::scripted(script.into_iter().map(Answer::Batch).collect())
        }

        fn scripted(script: Vec<Answer>) -> Arc<Self> {
            Arc::new(Self {
                base_url: "http://127.0.0.1:9".to_string(),
                pulls: Mutex::new(Vec::new()),
                reports: Mutex::new(Vec::new()),
                script: Mutex::new(script),
                taken: Mutex::new(HashSet::new()),
                seq: AtomicUsize::new(0),
                generation: Mutex::new(1),
                registrations: AtomicUsize::new(0),
                re_enrol_attempts: AtomicUsize::new(0),
                refuse_registration: std::sync::atomic::AtomicBool::new(false),
            })
        }
    }

    impl AssignmentMailbox for RecordingMailbox {
        fn session_ref(&self) -> String {
            "9b2e0f7c".to_string()
        }

        fn base_url(&self) -> String {
            self.base_url.clone()
        }

        fn pull(&self, ack: Vec<String>) -> MailboxFuture<'_, AssignmentPull> {
            self.pulls.lock().push(ack);
            let answer = {
                let mut script = self.script.lock();
                if script.is_empty() {
                    Answer::Batch(Vec::new())
                } else {
                    script.remove(0)
                }
            };
            Box::pin(async move {
                match answer {
                    Answer::Batch(assignments) => Ok(AssignmentPull {
                        messages: Vec::new(),
                        assignments,
                        acked: 0,
                        requeued: 0,
                        queued_remaining: 0,
                    }),
                    Answer::Refuse(reason) => Err(anyhow::anyhow!("{reason}")),
                    Answer::ForgotTheSession => Err(refusal(
                        404,
                        "unknown_session",
                        "no chat session with that id is registered; `GET /api/chat-sessions` lists the live ones",
                    )),
                    Answer::ForgotTheAssignment => Err(refusal(
                        404,
                        "unknown_assignment",
                        "that assignment is not outstanding for this session",
                    )),
                }
            })
        }

        fn report(
            &self,
            assignment_id: String,
            status: ResultStatus,
            summary: String,
        ) -> MailboxFuture<'_, AssignmentReceipt> {
            self.reports.lock().push((assignment_id.clone(), status, summary));
            let seq = self.seq.fetch_add(1, Ordering::SeqCst) as u64 + 1;
            Box::pin(async move {
                Ok(AssignmentReceipt {
                    assignment_id,
                    status: status.as_str().to_string(),
                    seq,
                })
            })
        }

        fn accept(&self, assignment_id: &str) -> bool {
            self.taken.lock().insert(assignment_id.to_string())
        }

        fn enrolment_generation(&self) -> u64 {
            *self.generation.lock()
        }

        fn re_enrol(&self, stale: u64) -> MailboxFuture<'_, ReEnrolment> {
            self.re_enrol_attempts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if self.refuse_registration.load(Ordering::SeqCst) {
                    anyhow::bail!("cannot reach a running PRX process at {}", self.base_url);
                }
                let mut generation = self.generation.lock();
                if *generation != stale {
                    return Ok(ReEnrolment::Superseded {
                        session_ref: "9b2e0f7c".to_string(),
                    });
                }
                *generation = stale + 1;
                self.registrations.fetch_add(1, Ordering::SeqCst);
                Ok(ReEnrolment::Registered {
                    session_ref: format!("re-enrolled-{}", *generation),
                })
            })
        }
    }

    #[tokio::test]
    async fn a_batch_is_acknowledged_only_after_it_reaches_the_local_queue() {
        let client = client_for_tests();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::channels::traits::ChannelMessage>(4);
        let tx: crate::runtime::h2_entry::ChatInputSender = tx.into();
        let outcome = take_batch(
            client.as_ref(),
            vec![assignment("a-1", "summarise the repo", "queue")],
            &tx,
            silent().as_ref(),
        )
        .await;
        assert_eq!(outcome.delivered, vec!["a-1".to_string()]);
        assert_eq!(
            outcome.acks,
            vec!["a-1".to_string()],
            "an assignment safely in the local queue is acknowledged on the next pull"
        );
        assert!(rx.try_recv().is_ok(), "the assignment must reach the chat input queue");

        // The same call with nowhere to deliver must acknowledge nothing: work
        // this chat cannot run has to stay the daemon's.
        let client2 = client_for_tests();
        let (closed_tx, closed_rx) = tokio::sync::mpsc::channel::<crate::channels::traits::ChannelMessage>(1);
        let closed_tx: crate::runtime::h2_entry::ChatInputSender = closed_tx.into();
        drop(closed_rx);
        let refused = take_batch(
            client2.as_ref(),
            vec![assignment("a-2", "do a thing", "queue")],
            &closed_tx,
            silent().as_ref(),
        )
        .await;
        assert!(refused.input_closed);
        assert!(
            refused.acks.is_empty(),
            "work that never reached the local queue must not be acknowledged: {:?}",
            refused.acks
        );
    }

    #[tokio::test]
    async fn a_redelivered_assignment_is_acknowledged_but_never_run_twice() {
        let client = client_for_tests();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::channels::traits::ChannelMessage>(8);
        let tx: crate::runtime::h2_entry::ChatInputSender = tx.into();
        let first = take_batch(
            client.as_ref(),
            vec![assignment("dup", "work", "queue")],
            &tx,
            silent().as_ref(),
        )
        .await;
        assert_eq!(first.delivered, vec!["dup".to_string()]);

        let mut again = assignment("dup", "work", "queue");
        again.deliveries = 2;
        let second = take_batch(client.as_ref(), vec![again], &tx, silent().as_ref()).await;
        assert!(
            second.delivered.is_empty(),
            "a redelivery of held work must not be queued again: {:?}",
            second.delivered
        );
        assert_eq!(second.duplicates, vec!["dup".to_string()]);
        assert_eq!(
            second.acks,
            vec!["dup".to_string()],
            "a redelivery must still be acknowledged, or the daemon keeps resending it"
        );
        assert!(rx.try_recv().is_ok());
        assert!(
            rx.try_recv().is_err(),
            "exactly one input message may exist for one assignment id"
        );
    }

    #[tokio::test]
    async fn the_first_pull_acknowledges_nothing_and_the_second_acknowledges_the_first_batch() {
        let mailbox = RecordingMailbox::new(vec![vec![assignment("a-1", "one", "queue")], vec![]]);
        let client = client_for_tests();
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::channels::traits::ChannelMessage>(8);
        let tx: crate::runtime::h2_entry::ChatInputSender = tx.into();

        // Drive the same two steps the loop drives, without its sleep.
        let mut pending: Vec<String> = Vec::new();
        for _ in 0..2u8 {
            let batch = mailbox.pull(pending.clone()).await.unwrap();
            pending.clear();
            let outcome = take_batch(client.as_ref(), batch.assignments, &tx, silent().as_ref()).await;
            pending = outcome.acks;
        }
        let pulls = mailbox.pulls.lock().clone();
        assert_eq!(pulls.len(), 2);
        assert!(
            pulls[0].is_empty(),
            "the first pull cannot acknowledge anything it has not received yet: {:?}",
            pulls[0]
        );
        assert_eq!(
            pulls[1],
            vec!["a-1".to_string()],
            "the batch taken by the first pull is acknowledged by the second"
        );
    }

    #[tokio::test]
    async fn a_dropped_turn_guard_reports_a_failure_rather_than_going_quiet() {
        let mailbox = RecordingMailbox::new(vec![]);
        let registry = super::super::daemon::new_in_flight_registry();
        {
            let _guard = AssignmentTurnGuard::new(
                Arc::clone(&mailbox) as Arc<dyn AssignmentMailbox>,
                Arc::clone(&registry),
                silent(),
                "a-1".to_string(),
            );
        }
        tokio::task::yield_now().await;
        let reports = mailbox.reports.lock().clone();
        assert_eq!(
            reports.len(),
            1,
            "a turn that ended without a reply still owes an answer"
        );
        assert_eq!(reports[0].0, "a-1");
        assert_eq!(reports[0].1, ResultStatus::Failed);
        assert!(!reports[0].2.trim().is_empty(), "expected a non-empty value");
    }

    #[tokio::test]
    async fn a_completed_turn_reports_its_reply() {
        let mailbox = RecordingMailbox::new(vec![]);
        let registry = super::super::daemon::new_in_flight_registry();
        {
            let mut guard = AssignmentTurnGuard::new(
                Arc::clone(&mailbox) as Arc<dyn AssignmentMailbox>,
                Arc::clone(&registry),
                silent(),
                "a-2".to_string(),
            );
            guard.completed("37 TODOs");
        }
        tokio::task::yield_now().await;
        let reports = mailbox.reports.lock().clone();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].1, ResultStatus::Completed);
        assert_eq!(reports[0].2, "37 TODOs");
    }

    /// D-053 / REDACT-TURN: the daemon stores an assignment report as the
    /// assignment's result, so the reply and the failure reason are redacted
    /// before they leave the chat. Fails before REDACT-TURN.
    #[tokio::test]
    async fn an_assignment_report_carries_no_secret_from_the_reply_or_the_failure() {
        use crate::agent::terminal::persistence_probe::PROBE_SECRET;
        let mailbox = RecordingMailbox::new(vec![]);
        let registry = super::super::daemon::new_in_flight_registry();
        {
            let mut completed = AssignmentTurnGuard::new(
                Arc::clone(&mailbox) as Arc<dyn AssignmentMailbox>,
                Arc::clone(&registry),
                silent(),
                "a-secret-1".to_string(),
            );
            completed.completed(&format!("Here is the key {PROBE_SECRET} you asked for"));
            let mut failed = AssignmentTurnGuard::new(
                Arc::clone(&mailbox) as Arc<dyn AssignmentMailbox>,
                Arc::clone(&registry),
                silent(),
                "a-secret-2".to_string(),
            );
            failed.failed(&format!("redux driver failed: upstream rejected key {PROBE_SECRET}"));
        }
        tokio::task::yield_now().await;
        let reports = mailbox.reports.lock().clone();
        assert_eq!(reports.len(), 2, "both turns report");
        for report in &reports {
            assert!(!report.2.contains(PROBE_SECRET), "{report:?}");
            assert!(report.2.contains("[REDACTED]"), "{report:?}");
        }
    }

    #[tokio::test]
    async fn a_disarmed_guard_reports_nothing_because_the_work_still_has_to_run() {
        let mailbox = RecordingMailbox::new(vec![]);
        let registry = super::super::daemon::new_in_flight_registry();
        {
            let mut guard = AssignmentTurnGuard::new(
                Arc::clone(&mailbox) as Arc<dyn AssignmentMailbox>,
                Arc::clone(&registry),
                silent(),
                "a-3".to_string(),
            );
            guard.disarm();
            assert!(!guard.is_armed());
        }
        tokio::task::yield_now().await;
        assert!(
            mailbox.reports.lock().is_empty(),
            "a requeued assignment must not be answered for before it runs"
        );
    }

    /// Collects every line the poller put on screen.
    fn recording_notifier() -> (Arc<dyn Fn(&str) + Send + Sync>, Arc<Mutex<Vec<String>>>) {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let notify: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |text: &str| sink.lock().push(text.to_string()));
        (notify, lines)
    }

    /// Lines the poller printed to say the mailbox is failing, for a reason
    /// containing `marker`. The recovery line quotes the reason it is about, so
    /// counting occurrences of the reason alone would count that too.
    fn complaints_about(said: &[String], marker: &str) -> usize {
        said.iter()
            .filter(|line| line.starts_with("Daemon assignments are not being received") && line.contains(marker))
            .count()
    }

    /// Drive the poller over a script and hand back everything it said.
    ///
    /// `start_paused` because the loop sleeps a poll interval between pulls: the
    /// interval is a sampling rate, and simulating it is what keeps a test about
    /// twenty pulls from taking thirty seconds.
    async fn lines_from_script(script: Vec<Answer>) -> Vec<String> {
        drive_script(RecordingMailbox::scripted(script)).await.1
    }

    /// Drive a prepared mailbox to the end of its script, handing back the
    /// mailbox and everything the poller said.
    async fn drive_script(mailbox: Arc<RecordingMailbox>) -> (Arc<RecordingMailbox>, Vec<String>) {
        let pulls_scripted = mailbox.script.lock().len();
        let (notify, lines) = recording_notifier();
        let registry = super::super::daemon::new_in_flight_registry();
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::channels::traits::ChannelMessage>(8);
        let tx: crate::runtime::h2_entry::ChatInputSender = tx.into();
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_poller(
            Arc::clone(&mailbox) as Arc<dyn AssignmentMailbox>,
            registry,
            tx,
            notify,
            cancel.clone(),
        ));
        for _ in 0..(pulls_scripted * 4 + 8) {
            if mailbox.pulls.lock().len() > pulls_scripted {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        cancel.cancel();
        handle.await.expect("the poller must end when it is cancelled");
        assert!(
            mailbox.pulls.lock().len() > pulls_scripted,
            "the script must have been driven to the end"
        );
        let said = lines.lock().clone();
        (mailbox, said)
    }

    const RATE_LIMITED: &str = "chat assignment pull failed with HTTP 429 Too Many Requests: \
                                Too many API requests. Please retry later.";
    const UNREACHABLE: &str = "cannot reach a running PRX process at http://127.0.0.1:9";

    /// A daemon restart forgets every registration, and this chat holds a
    /// session id nothing answers to. Registering again is the recovery, and it
    /// happens here rather than in whoever notices the chat stopped working.
    #[tokio::test(start_paused = true)]
    async fn a_daemon_that_forgot_this_session_is_answered_by_registering_again() {
        let (mailbox, said) = drive_script(RecordingMailbox::scripted(vec![
            Answer::Batch(vec![]),
            Answer::ForgotTheSession,
            Answer::Batch(vec![assignment("after-restart", "count the crates", "queue")]),
        ]))
        .await;
        assert_eq!(
            mailbox.registrations.load(Ordering::SeqCst),
            1,
            "the poller must register again by itself; said:\n{said:#?}"
        );
        assert_eq!(*mailbox.generation.lock(), 2, "the new enrolment replaces the old one");
        assert!(
            mailbox.taken.lock().contains("after-restart"),
            "the point of re-enrolling is taking work again, and this chat took none"
        );
        assert!(
            said.iter().any(|line| line.contains("registered again")),
            "a new session id is a fact the operator has to be told; said:\n{said:#?}"
        );
        assert!(
            !said.iter().any(|line| line.contains("are not being received")),
            "a restart that healed itself must not be reported as a failure; said:\n{said:#?}"
        );
    }

    /// The acknowledgements from before the restart belong to a mailbox that no
    /// longer exists; sending them on would acknowledge ids the new session
    /// never handed out.
    #[tokio::test(start_paused = true)]
    async fn work_acknowledged_to_the_old_session_is_not_acknowledged_to_the_new_one() {
        let (mailbox, _said) = drive_script(RecordingMailbox::scripted(vec![
            Answer::Batch(vec![assignment("before-restart", "work", "queue")]),
            Answer::ForgotTheSession,
            Answer::Batch(vec![]),
        ]))
        .await;
        let acks = mailbox.pulls.lock().clone();
        assert_eq!(
            acks.first().map(Vec::len),
            Some(0),
            "the first pull acknowledges nothing: {acks:?}"
        );
        assert_eq!(
            acks.get(1).map(Vec::as_slice),
            Some(["before-restart".to_string()].as_slice()),
            "the pull that met the restart still carried the acknowledgement it owed: {acks:?}"
        );
        assert!(
            acks.iter().skip(2).all(Vec::is_empty),
            "a new session must not be handed the dead session's acknowledgements: {acks:?}"
        );
    }

    /// A daemon stuck in "who are you?" answers every single pull that way. The
    /// recovery is idempotent, but every successful one puts a line on the
    /// operator's screen, so re-registering on each answer would write one
    /// every poll interval — a notification flood from a loop that is supposed
    /// to be quiet.
    ///
    /// MUTATION GUARD: remove the [`RE_ENROLMENT_MIN_INTERVAL`] gate and this
    /// fails with one attempt (and one notice) per unregistered answer.
    #[tokio::test(start_paused = true)]
    async fn a_daemon_that_keeps_forgetting_this_session_is_answered_at_most_once_a_minute() {
        let (mailbox, said) = drive_script(RecordingMailbox::scripted(vec![
            Answer::ForgotTheSession,
            Answer::ForgotTheSession,
            Answer::ForgotTheSession,
            Answer::ForgotTheSession,
            Answer::ForgotTheSession,
            Answer::ForgotTheSession,
        ]))
        .await;
        assert_eq!(
            mailbox.re_enrol_attempts.load(Ordering::SeqCst),
            1,
            "six unregistered answers inside one minute are one recovery attempt; said:\n{said:#?}"
        );
        assert_eq!(
            said.iter().filter(|line| line.contains("registered again")).count(),
            1,
            "and therefore exactly one notice; said:\n{said:#?}"
        );
        assert!(
            mailbox.pulls.lock().len() > 6,
            "the poller must keep polling while the recovery is rate limited: {:?}",
            mailbox.pulls.lock().len()
        );
    }

    /// The same status, a different fact. An unknown *assignment* says nothing
    /// about whether this session is registered, and re-registering on it would
    /// abandon a perfectly good enrolment.
    #[tokio::test(start_paused = true)]
    async fn a_refusal_about_an_assignment_never_makes_this_chat_register_again() {
        let (mailbox, said) = drive_script(RecordingMailbox::scripted(vec![
            Answer::ForgotTheAssignment,
            Answer::ForgotTheAssignment,
            Answer::Batch(vec![]),
        ]))
        .await;
        assert_eq!(
            mailbox.registrations.load(Ordering::SeqCst),
            0,
            "only `unknown_session` means this session is gone; said:\n{said:#?}"
        );
        assert!(
            said.iter().any(|line| line.contains("are not being received")),
            "a refusal this chat cannot heal must still be stated; said:\n{said:#?}"
        );
    }

    /// A daemon that is gone rather than merely forgetful: registering again
    /// fails too, a later pull is the retry, and it costs one line however long
    /// the daemon stays down. The retry is rate limited (see
    /// [`RE_ENROLMENT_MIN_INTERVAL`]), so the poller keeps polling while the
    /// recovery attempt itself happens at most once a minute.
    #[tokio::test(start_paused = true)]
    async fn a_registration_that_also_fails_is_retried_later_and_said_once() {
        let mailbox = RecordingMailbox::scripted(vec![
            Answer::ForgotTheSession,
            Answer::ForgotTheSession,
            Answer::ForgotTheSession,
        ]);
        mailbox.refuse_registration.store(true, Ordering::SeqCst);
        let (mailbox, said) = drive_script(mailbox).await;
        assert_eq!(mailbox.registrations.load(Ordering::SeqCst), 0);
        assert!(
            mailbox.pulls.lock().len() >= 3,
            "the poll loop is the retry; it must keep asking: {:?}",
            mailbox.pulls.lock().len()
        );
        assert_eq!(
            mailbox.re_enrol_attempts.load(Ordering::SeqCst),
            1,
            "three refusals inside one minute buy one recovery attempt, not three"
        );
        assert_eq!(
            complaints_about(&said, "cannot reach"),
            1,
            "one blocked recovery, one line; said:\n{said:#?}"
        );
    }

    /// Idempotency, at the level it actually matters: two callers that saw the
    /// same dead enrolment must produce one registration, not two sessions the
    /// daemon then has to be told apart.
    #[tokio::test]
    async fn concurrent_recoveries_from_the_same_dead_enrolment_register_once() {
        let mailbox = RecordingMailbox::scripted(vec![]);
        let stale = mailbox.enrolment_generation();
        let first = mailbox.re_enrol(stale);
        let second = mailbox.re_enrol(stale);
        let third = mailbox.re_enrol(stale);
        let (first, second, third) = tokio::join!(first, second, third);
        let outcomes = [
            first.expect("re-enrolment"),
            second.expect("re-enrolment"),
            third.expect("re-enrolment"),
        ];
        let registered = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ReEnrolment::Registered { .. }))
            .count();
        assert_eq!(registered, 1, "three triggers, one registration: {outcomes:?}");
        assert_eq!(mailbox.registrations.load(Ordering::SeqCst), 1);
    }

    /// The flood this was written against: a mailbox that refuses every other
    /// pull. The reason is identical every time, so it is worth exactly one
    /// line however many times it comes back.
    ///
    /// MUTATION GUARD: clear `FailureNotices::announced` on a successful pull —
    /// the shape of the dedup this replaced — and this fails with one line per
    /// refusal.
    #[tokio::test(start_paused = true)]
    async fn one_failure_reason_is_stated_once_however_often_it_recurs() {
        let said = lines_from_script(vec![
            Answer::Refuse(RATE_LIMITED),
            Answer::Batch(vec![]),
            Answer::Refuse(RATE_LIMITED),
            Answer::Batch(vec![]),
            Answer::Refuse(RATE_LIMITED),
            Answer::Refuse(RATE_LIMITED),
            Answer::Batch(vec![]),
            Answer::Refuse(RATE_LIMITED),
        ])
        .await;
        assert_eq!(
            complaints_about(&said, "429"),
            1,
            "one reason is worth one line, not one per failing pull; said:\n{said:#?}"
        );
        let recoveries = said.iter().filter(|line| line.contains("being received again")).count();
        assert_eq!(
            recoveries, 1,
            "recovery is announced, and it is announced once; said:\n{said:#?}"
        );
    }

    /// Going quiet about a failure is the other half of the bug: the operator
    /// has to be told, once, and told again when it clears.
    #[tokio::test(start_paused = true)]
    async fn a_failure_and_its_recovery_are_both_stated() {
        let said = lines_from_script(vec![
            Answer::Refuse(UNREACHABLE),
            Answer::Refuse(UNREACHABLE),
            Answer::Batch(vec![]),
        ])
        .await;
        assert!(
            said.iter().any(|line| line.contains("are not being received")),
            "a mailbox that cannot be reached must say so; said:\n{said:#?}"
        );
        let recovery = said
            .iter()
            .find(|line| line.contains("being received again"))
            .expect("recovery must be announced, or nobody knows when it healed");
        assert!(
            recovery.contains("2 failed pulls"),
            "the recovery line says how long it was broken: {recovery}"
        );
    }

    /// A different reason is a different fact, and is stated on its own.
    #[tokio::test(start_paused = true)]
    async fn a_changed_reason_is_stated_even_while_the_mailbox_is_still_failing() {
        let said = lines_from_script(vec![
            Answer::Refuse(RATE_LIMITED),
            Answer::Refuse(RATE_LIMITED),
            Answer::Refuse(UNREACHABLE),
            Answer::Refuse(UNREACHABLE),
        ])
        .await;
        assert_eq!(complaints_about(&said, "429"), 1, "said:\n{said:#?}");
        assert_eq!(
            complaints_about(&said, "cannot reach"),
            1,
            "a reason the operator has not seen must not be swallowed by the previous one; said:\n{said:#?}"
        );
    }

    #[test]
    fn a_recovery_is_never_announced_for_a_failure_that_was_never_announced() {
        let mut notices = FailureNotices::default();
        assert!(notices.recovered().is_none(), "nothing has failed yet");
        assert!(notices.failed("reason a").is_some());
        assert!(notices.failed("reason a").is_none());
        assert!(notices.recovered().is_some());
        // Second episode of the same reason: silent, so its recovery is silent
        // too. Announcing "it works again" for something never reported broken
        // is noise about a fact nobody was missing.
        assert!(notices.failed("reason a").is_none());
        assert!(notices.recovered().is_none());
        assert!(!notices.is_failing());
    }

    #[test]
    fn a_peer_whose_wording_keeps_changing_cannot_grow_the_policy_without_bound() {
        let mut notices = FailureNotices::default();
        let mut announced = 0;
        for attempt in 0..(MAX_TRACKED_REASONS * 4) {
            if notices.failed(&format!("refused, attempt {attempt}")).is_some() {
                announced += 1;
            }
        }
        assert_eq!(
            announced, MAX_TRACKED_REASONS,
            "a varying message must not buy unlimited screen space"
        );
    }

    #[tokio::test]
    async fn the_poller_is_listed_while_it_runs_and_a_kill_ends_it() {
        let registry = super::super::daemon::new_in_flight_registry();
        let client = client_for_tests();
        let (tx, _rx) = tokio::sync::mpsc::channel::<crate::channels::traits::ChannelMessage>(8);
        let tx: crate::runtime::h2_entry::ChatInputSender = tx.into();
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_poller(
            Arc::clone(&client) as Arc<dyn AssignmentMailbox>,
            Arc::clone(&registry),
            tx,
            silent(),
            cancel.clone(),
        ));
        // The row is published before the first request, so it is visible even
        // while the very first pull is outstanding.
        let mut listed = None;
        for _ in 0..100u16 {
            if let Some(report) = super::super::daemon::in_flight_report(&registry) {
                listed = Some(report);
                break;
            }
            tokio::task::yield_now().await;
        }
        let listed = listed.expect("the poller must be listed in /sessions while it runs");
        assert!(listed.contains("assignment poll"), "listing was: {listed}");
        let killed = super::super::daemon::cancel_in_flight(&registry, "d1").expect("d1 is this chat's address space");
        assert!(killed.contains("d1"), "kill notice was: {killed}");
        handle.await.expect("the poller must end when it is killed");
        assert!(
            super::super::daemon::in_flight_report(&registry).is_none(),
            "an ended poller must stop being listed"
        );
    }

    #[test]
    fn an_assignment_survives_a_round_trip_through_the_input_queue() {
        for (wire, disposition) in [
            ("queue", Disposition::Queue),
            ("steer", Disposition::Steer),
            ("interrupt", Disposition::Interrupt),
        ] {
            let msg = assignment_message(&assignment("a-1", "do the thing", wire), disposition);
            let seen = delivered(&msg).expect("an assignment message must be recognisable");
            assert_eq!(seen.assignment_id, "a-1");
            assert_eq!(seen.disposition, disposition);
            if disposition.jumps_the_queue() {
                assert!(
                    msg.content.starts_with("/now "),
                    "a queue-jumping disposition must use the existing priority prefix: {}",
                    msg.content
                );
            } else {
                assert_eq!(msg.content, "do the thing");
            }
        }
    }

    #[test]
    fn ordinary_input_is_never_mistaken_for_an_assignment() {
        let mut msg = assignment_message(&assignment("a-1", "task", "queue"), Disposition::Queue);
        msg.sender = "user".to_string();
        assert!(delivered(&msg).is_none());
        msg.sender = "prx-ui".to_string();
        assert!(delivered(&msg).is_none());
        msg.sender = format!("{SENDER_PREFIX}queue:");
        assert!(delivered(&msg).is_none(), "an empty assignment id is not an assignment");
    }

    #[test]
    fn an_unknown_disposition_degrades_to_queue_and_never_to_interrupt() {
        assert_eq!(Disposition::from_wire("obliterate"), Disposition::Queue);
        assert_eq!(Disposition::from_wire(""), Disposition::Queue);
        assert!(!Disposition::is_known_wire("obliterate"));
        assert!(Disposition::is_known_wire("interrupt"));
        assert!(!Disposition::from_wire("obliterate").ends_the_turn_in_flight());
    }

    #[test]
    fn only_interrupt_ends_the_turn_in_flight() {
        assert!(!Disposition::Queue.jumps_the_queue());
        assert!(Disposition::Steer.jumps_the_queue());
        assert!(Disposition::Interrupt.jumps_the_queue());
        assert!(!Disposition::Queue.ends_the_turn_in_flight());
        assert!(
            !Disposition::Steer.ends_the_turn_in_flight(),
            "steer inserts into the turn in flight; it does not destroy it"
        );
        assert!(Disposition::Interrupt.ends_the_turn_in_flight());
    }

    #[test]
    fn a_chat_session_can_never_report_the_daemons_own_cancelled_verdict() {
        let reportable: Vec<&str> = ResultStatus::all().iter().map(|status| status.as_str()).collect();
        assert_eq!(reportable, vec!["completed", "failed", "rejected"]);
        assert!(
            !reportable.contains(&"cancelled"),
            "`cancelled` is the daemon's verdict about work it ended, not something a client may claim"
        );
    }

    #[test]
    fn a_summary_is_fitted_to_what_the_daemon_accepts() {
        assert_eq!(bound_summary("  answer  "), "answer");
        assert!(
            !bound_summary("   ").is_empty(),
            "an empty summary is refused by the daemon"
        );
        let huge = "x".repeat(MAX_SUMMARY_BYTES * 2);
        let bounded = bound_summary(&huge);
        assert!(bounded.len() <= MAX_SUMMARY_BYTES, "bounded to {}", bounded.len());
        assert!(bounded.ends_with("to fit the result field)"));
        let wide = "\u{20ac}".repeat(MAX_SUMMARY_BYTES);
        assert!(bound_summary(&wide).len() <= MAX_SUMMARY_BYTES);
    }

    #[test]
    fn enrolment_is_declined_when_nothing_points_this_chat_at_a_daemon() {
        let mut config = Config::default();
        config.chat.daemon.url = String::new();
        config.chat.daemon.token = String::new();
        config.gateway.paired_tokens = vec![];
        match enrolable(&config) {
            Enrolable::No(reason) => {
                assert!(reason.contains("chat.daemon.url"), "reason was: {reason}");
            }
            Enrolable::Yes => panic!("an unconfigured chat must not announce a daemon failure on every start"),
        }
        config.chat.daemon.url = "http://127.0.0.1:8765".to_string();
        assert_eq!(enrolable(&config), Enrolable::Yes);

        let mut with_token = Config::default();
        with_token.chat.daemon.url = String::new();
        with_token.chat.daemon.token = "plaintext-token".to_string();
        assert_eq!(enrolable(&with_token), Enrolable::Yes);
    }

    #[test]
    fn an_arrival_names_the_origin_and_the_disposition_without_going_quiet() {
        let notice = arrival_notice(
            &assignment("a-1", "summarise the repo", "interrupt"),
            Disposition::Interrupt,
        );
        assert!(notice.contains("wacli"), "{notice}");
        assert!(notice.contains("3d1a4f5b"), "{notice}");
        assert!(notice.contains("summarise the repo"), "{notice}");
        assert!(notice.contains("interrupt"), "{notice}");

        let mut redelivered = assignment("a-1", "task", "queue");
        redelivered.deliveries = 3;
        assert!(arrival_notice(&redelivered, Disposition::Queue).contains("redelivery #3"));

        let mut operator = assignment("a-2", "task", "queue");
        operator.origin_channel = "api".to_string();
        operator.origin_ref = None;
        let plane = arrival_notice(&operator, Disposition::Queue);
        assert!(plane.contains("api"), "{plane}");
    }

    #[test]
    fn a_label_is_stable_and_fits_what_the_daemon_accepts() {
        let mut config = Config::default();
        config.workspace_dir = std::path::PathBuf::from("/tmp/some/workstation");
        assert_eq!(label_for(&config), "workstation");
        config.workspace_dir = std::path::PathBuf::from(format!("/tmp/{}", "n".repeat(200)));
        assert!(label_for(&config).len() <= MAX_LABEL_BYTES);
        config.workspace_dir = std::path::PathBuf::from("/");
        assert_eq!(label_for(&config), "chat");
    }
    /// Mutation: retain a raw chain in an assignment notice or repeated-pull episode.
    #[test]
    fn assignment_failure_notices_hide_nested_credentials() {
        let error = anyhow::anyhow!("token=opaque7credential").context("failed");
        let mut notices = FailureNotices::default();
        let pull = notices.failed(&format!("{error:#}")).expect("test: first notice");
        for notice in [
            report_failure_notice("assignment", ResultStatus::Failed, &error),
            enrolment_failure_notice("https://example.com?key=opaque7credential", &error),
            pull,
        ] {
            assert!(notice.contains("[REDACTED]"), "{notice}");
            assert!(!notice.contains("opaque7credential"), "{notice}");
        }
    }
}
