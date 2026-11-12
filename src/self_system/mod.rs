pub mod evolution;
pub mod fitness;
pub mod fitness_store;

pub const SELF_SYSTEM_SESSION_ID: &str = "self_system";

pub use fitness::run_fitness_report_with_config;
