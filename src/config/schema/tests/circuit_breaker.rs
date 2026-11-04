//! Breaker defaults and top-level configuration validation.
use super::*;

#[test]
fn circuit_breaker_defaults_and_absent_table() {
    let absent: Config = toml::from_str("").unwrap();
    let config = Config::default();
    assert_eq!(absent.reliability.circuit_breaker, config.reliability.circuit_breaker);
    let breaker = &config.reliability.circuit_breaker;
    assert!(breaker.enabled);
    assert_eq!(breaker.failure_threshold, 5);
    assert_eq!(breaker.open_secs, 30);
    assert_eq!(breaker.max_open_secs, 300);
}
#[test]
fn circuit_breaker_invalid_values_rejected_by_config_validate() {
    for (key, value) in [
        ("failure_threshold", 0),
        ("open_secs", 0),
        ("max_open_secs", 29),
        ("max_open_secs", 3601),
    ] {
        let text = format!("[reliability.circuit_breaker]\n{key} = {value}\n");
        let config: Config = toml::from_str(&text).unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains(&format!("reliability.circuit_breaker.{key}")), "{error}");
        assert!(error.contains("fix the key or delete"), "{error}");
    }
}
