wit_bindgen::generate!({ path: "wit", world: "channel" });
struct EchoChannel;
static POLLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
impl exports::prx::plugin::channel_exports::Guest for EchoChannel {
    fn send(message: String) -> Result<String, String> {
        if message.contains("PRX_TRAP_PROBE") { panic!("fixture trap"); }
        Ok("ack".into())
    }
    fn poll(_request: String) -> Result<String, String> {
        if POLLED.swap(true, std::sync::atomic::Ordering::SeqCst) { return Ok("[]".into()); }
        Ok(r#"[{"id":"prx-message","sender":"prx_user","reply_target":"prx_chat","content":"channel fixture"},{"id":"prx-denied","sender":"prx_other","reply_target":"prx_chat","content":"must be filtered"}]"#.into())
    }
}
export!(EchoChannel);
