#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

mod retry_and_backoff;
mod stream_release;
mod stream_retry;

use retry_and_backoff::*;
use stream_retry::*;
