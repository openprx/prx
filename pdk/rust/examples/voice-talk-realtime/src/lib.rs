use prx_pdk::prelude::*;

#[allow(warnings)]
mod bindings;

// ── Constants ───────────────────────────────────────────────────────

const OPENAI_WS_URL: &str = "wss://api.openai.com/v1/realtime";
const XAI_WS_URL: &str = "wss://api.x.ai/v1/realtime";

const OPENAI_DEFAULT_MODEL: &str = "gpt-4o-realtime-preview-2024-12-17";
const XAI_DEFAULT_MODEL: &str = "grok-3-fast-realtime";

const OPENAI_DEFAULT_VOICE: &str = "verse";
const XAI_DEFAULT_VOICE: &str = "eve";

const OPENAI_VOICES: &[&str] = &["alloy", "ash", "ballad", "coral", "echo", "sage", "shimmer", "verse"];
const XAI_VOICES: &[&str] = &["eve", "ara", "rex", "sal", "leo"];

// ── Plugin implementation ───────────────────────────────────────────

pub struct VoiceTalkPlugin;

impl VoiceTalkPlugin {
    pub fn get_spec_impl() -> ToolSpec {
        ToolSpec {
            name: "voice_session".to_string(),
            description: "Create a real-time voice session configuration. Returns WebSocket \
                          connection details for xAI or OpenAI Realtime API."
                .to_string(),
            parameters_schema: r#"{
  "type": "object",
  "properties": {
    "provider": {
      "type": "string",
      "enum": ["openai", "xai"],
      "description": "Provider: 'openai' or 'xai'"
    },
    "voice": {
      "type": "string",
      "description": "Voice name (openai: alloy/ash/ballad/coral/echo/sage/shimmer/verse, xai: eve/ara/rex/sal/leo)"
    },
    "model": {
      "type": "string",
      "description": "Model override"
    },
    "instructions": {
      "type": "string",
      "description": "System instructions for the voice session"
    },
    "turn_detection": {
      "type": "string",
      "enum": ["server_vad", "none"],
      "description": "Turn detection mode"
    }
  },
  "required": ["provider"]
}"#
            .to_string(),
        }
    }

    pub fn execute_impl(args_json: &str) -> PluginResult {
        let args: JsonValue = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => return PluginResult::err(format!("Invalid JSON args: {e}")),
        };

        let provider = match args["provider"].as_str() {
            Some(s) => s.to_lowercase(),
            None => return PluginResult::err("Missing required 'provider' parameter"),
        };

        let (ws_url, default_model, default_voice, valid_voices, auth_method) = match provider.as_str() {
            "openai" => (OPENAI_WS_URL, OPENAI_DEFAULT_MODEL, OPENAI_DEFAULT_VOICE, OPENAI_VOICES, "header"),
            "xai" => (XAI_WS_URL, XAI_DEFAULT_MODEL, XAI_DEFAULT_VOICE, XAI_VOICES, "subprotocol"),
            _ => return PluginResult::err(format!("Unsupported provider '{}'. Use 'openai' or 'xai'", provider)),
        };

        let model = args["model"].as_str().unwrap_or(default_model);
        let voice = args["voice"].as_str().unwrap_or(default_voice);

        if !valid_voices.contains(&voice) {
            return PluginResult::err(format!(
                "Invalid voice '{}' for {}. Available: {:?}", voice, provider, valid_voices
            ));
        }

        let turn_detection = args["turn_detection"].as_str().unwrap_or("server_vad");
        let instructions = args["instructions"].as_str().unwrap_or("You are a helpful assistant.");

        let connection_url = match provider.as_str() {
            "openai" => format!("{}?model={}", ws_url, model),
            _ => ws_url.to_string(),
        };

        let session_config = match provider.as_str() {
            "openai" => json!({
                "modalities": ["text", "audio"],
                "voice": voice,
                "instructions": instructions,
                "input_audio_format": "pcm16",
                "output_audio_format": "pcm16",
                "turn_detection": if turn_detection == "none" { JsonValue::Null } else { json!({ "type": turn_detection }) }
            }),
            "xai" => json!({
                "voice": voice,
                "instructions": instructions,
                "input_audio_format": "pcm16",
                "output_audio_format": "pcm16",
                "turn_detection": if turn_detection == "none" { JsonValue::Null } else { json!({ "type": turn_detection }) },
                "tools": [{ "type": "web_search" }, { "type": "x_search" }],
                "input_audio_transcription": { "model": "grok-2-audio" }
            }),
            _ => json!({}),
        };

        // Track usage
        let _ = kv::increment(&format!("{}_session_count", provider), 1);
        log::info(&format!("voice_session: provider={} model={} voice={}", provider, model, voice));

        let result = json!({
            "status": "ok",
            "provider": provider,
            "connection": {
                "url": connection_url,
                "auth_method": auth_method,
                "model": model,
            },
            "session_config": session_config,
            "voices": valid_voices,
            "audio_format": {
                "encoding": "pcm16",
                "sample_rate": 24000,
                "channels": 1,
            }
        });

        PluginResult::ok(result.to_string())
    }
}

// ── WIT guest trait (wasm32 only) ───────────────────────────────────

#[cfg(target_arch = "wasm32")]
mod wasm_exports {
    use super::VoiceTalkPlugin;
    use super::bindings;
    use bindings::exports::prx::plugin::tool_exports::Guest;

    impl Guest for VoiceTalkPlugin {
        fn get_spec() -> bindings::exports::prx::plugin::tool_exports::ToolSpec {
            let s = VoiceTalkPlugin::get_spec_impl();
            bindings::exports::prx::plugin::tool_exports::ToolSpec {
                name: s.name,
                description: s.description,
                parameters_schema: s.parameters_schema,
            }
        }

        fn execute(
            args: String,
        ) -> bindings::exports::prx::plugin::tool_exports::PluginResult {
            let r = VoiceTalkPlugin::execute_impl(&args);
            bindings::exports::prx::plugin::tool_exports::PluginResult {
                success: r.success,
                output: r.output,
                error: r.error,
            }
        }
    }

    bindings::export!(VoiceTalkPlugin with_types_in bindings);
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_has_required_fields() {
        let spec = VoiceTalkPlugin::get_spec_impl();
        assert_eq!(spec.name, "voice_session");
        let schema: serde_json::Value = serde_json::from_str(&spec.parameters_schema).unwrap();
        assert!(schema["properties"]["provider"].is_object());
    }

    #[test]
    fn xai_session() {
        let r = VoiceTalkPlugin::execute_impl(r#"{"provider":"xai"}"#);
        assert!(r.success);
        let v: serde_json::Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["provider"], "xai");
        assert_eq!(v["connection"]["auth_method"], "subprotocol");
    }

    #[test]
    fn openai_session() {
        let r = VoiceTalkPlugin::execute_impl(r#"{"provider":"openai"}"#);
        assert!(r.success);
        let v: serde_json::Value = serde_json::from_str(&r.output).unwrap();
        assert_eq!(v["provider"], "openai");
        assert!(v["connection"]["url"].as_str().unwrap().contains("model="));
    }

    #[test]
    fn invalid_provider() {
        let r = VoiceTalkPlugin::execute_impl(r#"{"provider":"google"}"#);
        assert!(!r.success);
    }

    #[test]
    fn invalid_voice() {
        let r = VoiceTalkPlugin::execute_impl(r#"{"provider":"xai","voice":"invalid"}"#);
        assert!(!r.success);
    }
}
