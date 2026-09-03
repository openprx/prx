#[allow(warnings)]
#[cfg(target_arch = "wasm32")]
mod bindings;

struct PipelineMiddleware;

impl PipelineMiddleware {
    fn process(stage: &str, data_json: &str) -> Result<String, String> {
        let mut value: serde_json::Value = serde_json::from_str(data_json).map_err(|error| error.to_string())?;
        value["middleware_probe"] = serde_json::json!({"plugin": "pipeline-middleware", "stage": stage});
        if stage == "outbound" {
            if let Some(text) = value.get("text").and_then(serde_json::Value::as_str) {
                value["text"] = serde_json::Value::String(format!("[wasm-middleware] {text}"));
            }
        }
        serde_json::to_string(&value).map_err(|error| error.to_string())
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_exports {
    use super::{bindings, PipelineMiddleware};
    use bindings::exports::prx::plugin::middleware_exports::Guest;

    impl Guest for PipelineMiddleware {
        fn process(stage: String, data_json: String) -> Result<String, String> {
            PipelineMiddleware::process(&stage, &data_json)
        }
    }

    bindings::export!(PipelineMiddleware with_types_in bindings);
}

