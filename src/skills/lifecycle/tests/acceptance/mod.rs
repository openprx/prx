#![cfg(test)]

//! SL10: local end-to-end acceptance of the skill lifecycle (spec section 17,
//! scenarios A-E). The scenarios drive the production entry points: the
//! single-shot agent entry (`crate::agent::run`) against a loopback model,
//! `skill_read`, the task-terminal call-in, knowledge consolidation, the
//! proposer, the frozen-suite evaluator, the gate, the lifecycle service and
//! the `prx skills lifecycle` CLI handler.
//!
//! Nothing here claims that PRX learns (spec 11.1, invariant 11): the model is
//! a deterministic fixture and the proposer is scripted. The scenarios assert
//! the observable exit evidence of the mechanics. The learning claim and the
//! real surfaces stay `blocked-external` (see `scenario_e`).

mod flows;
mod scenario_a;
mod scenario_b;
mod scenario_c;
mod scenario_d;
mod scenario_e;
mod support;
