//! Daemon-assigned work: how `prx chat` enrols, takes work, and answers for it.
//!
//! The other direction of the control link. `daemon.rs` is chat asking the
//! daemon to do something; this module is the daemon asking *chat* to. A
//! message can arrive on WhatsApp naming a chat session and a task, and that
//! task has to run in **this** process — the daemon's own
//! `POST /api/sessions/{id}/message` starts a turn inside the daemon and has
//! nothing to do with a separate `prx chat`.
//!
//! Four decisions shape everything below.
//!
//! - **Chat pulls; nothing pushes.** Chat already speaks to the daemon's HTTP
//!   control API, so taking work over the same client needs no listener, no
//!   port and no second transport in a process whose whole job is a terminal.
//!   The poll interval is a *sampling rate*, not a deadline: nothing here
//!   measures elapsed time, caps iterations, or abandons a request.
//! - **Acknowledge on the next pull, never on receipt.** The mailbox is
//!   at-least-once. A client that acknowledges the moment bytes arrive and then
//!   dies has destroyed the work; a client that acknowledges on its *next* pull
//!   has proved it survived long enough to queue it. The cost is redelivery,
//!   which [`AssignmentClient::accept`] absorbs by treating `assignment_id` as
//!   an idempotency key.
//! - **Enrolment never blocks startup.** A daemon that is down, unreachable or
//!   refusing credentials must cost this chat exactly one capability — being
//!   assignable — and nothing else. The failure is stated once, in the
//!   transcript, and chat carries on.
//! - **A forgotten registration heals itself.** The daemon keeps its session
//!   registry in memory, so every daemon restart — an upgrade, a config change,
//!   a crash recovery — leaves every chat holding a session id nothing answers
//!   to. Chat notices that in the refusal it already makes a request for, and
//!   registers again; it does not wait for somebody to restart it. The retry is
//!   the next pull, because this loop is already periodic and nothing in this
//!   runtime is allowed a timer.
//! - **Assigned work is never silent.** Somebody else is driving this terminal.
//!   Every arrival is announced with where it came from and how it was
//!   dispatched, and every result reported back is announced too, so the person
//!   sitting in front of it can always account for what ran.
//!
//! What the three dispositions mean here is deliberately *not* new machinery:
//! the daemon only records a disposition and puts the item at the head of the
//! queue, and chat maps each one onto a path it already had.

include!("assignment/section_1.rs");
include!("assignment/section_2.rs");
