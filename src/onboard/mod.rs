pub mod wizard;

pub use wizard::run_models_refresh;

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_reexport_exists<F>(_value: F) {}

    #[test]
    fn wizard_functions_are_reexported() {
        assert_reexport_exists(wizard::run_wizard);
        assert_reexport_exists(wizard::run_channels_repair_wizard);
        assert_reexport_exists(wizard::run_quick_setup);
        assert_reexport_exists(run_models_refresh);
    }
}
