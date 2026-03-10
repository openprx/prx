use crate::agent::dispatcher::{
    NativeToolDispatcher, ParsedToolCall, ToolDispatcher, ToolExecutionResult, XmlToolDispatcher,
};
use crate::agent::memory_loader::{DefaultMemoryLoader, MemoryLoader};
use crate::agent::prompt::{PromptContext, SystemPromptBuilder};
use crate::config::Config;
use crate::hooks::{payload_error, HookEvent, HookManager};
use crate::memory::{self, Memory, MemoryCategory};
use crate::observability::{self, Observer, ObserverEvent};
use crate::providers::{self, ChatMessage, ChatRequest, ConversationMessage, Provider};
#[cfg(feature = "llm-router")]
use crate::router::RouterEngine;
#[cfg(feature = "llm-router")]
use crate::router::automix::{is_cheap_model_target, should_escalate, ConfidenceChecker};
use crate::runtime;
use crate::security::SecurityPolicy;
#[cfg(feature = "llm-router")]
use crate::self_system::SELF_SYSTEM_SESSION_ID;
use crate::tools::{self, Tool};
use anyhow::Result;
#[cfg(feature = "llm-router")]
use chrono::Utc;
use std::io::Write as IoWrite;
use std::sync::Arc;
use std::time::Instant;

pub struct Agent {
    provider: Box<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    memory: Arc<dyn Memory>,
    observer: Arc<dyn Observer>,
    hooks: Arc<HookManager>,
    prompt_builder: SystemPromptBuilder,
    tool_dispatcher: Box<dyn ToolDispatcher>,
    memory_loader: Box<dyn MemoryLoader>,
    config: crate::config::AgentConfig,
    model_name: String,
    temperature: f64,
    workspace_dir: std::path::PathBuf,
    identity_config: crate::config::IdentityConfig,
    skills: Vec<crate::skills::Skill>,
    auto_save: bool,
    history: Vec<ConversationMessage>,
    classification_config: crate::config::QueryClassificationConfig,
    task_routing_config: crate::config::TaskRoutingConfig,
    available_hints: Vec<String>,
    #[cfg(feature = "llm-router")]
    router: Option<RouterEngine>,
}

pub struct AgentBuilder {
    provider: Option<Box<dyn Provider>>,
    tools: Option<Vec<Box<dyn Tool>>>,
    memory: Option<Arc<dyn Memory>>,
    observer: Option<Arc<dyn Observer>>,
    hooks: Option<Arc<HookManager>>,
    prompt_builder: Option<SystemPromptBuilder>,
    tool_dispatcher: Option<Box<dyn ToolDispatcher>>,
    memory_loader: Option<Box<dyn MemoryLoader>>,
    config: Option<crate::config::AgentConfig>,
    model_name: Option<String>,
    temperature: Option<f64>,
    workspace_dir: Option<std::path::PathBuf>,
    identity_config: Option<crate::config::IdentityConfig>,
    skills: Option<Vec<crate::skills::Skill>>,
    auto_save: Option<bool>,
    classification_config: Option<crate::config::QueryClassificationConfig>,
    task_routing_config: Option<crate::config::TaskRoutingConfig>,
    available_hints: Option<Vec<String>>,
    #[cfg(feature = "llm-router")]
    router: Option<RouterEngine>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            provider: None,
            tools: None,
            memory: None,
            observer: None,
            hooks: None,
            prompt_builder: None,
            tool_dispatcher: None,
            memory_loader: None,
            config: None,
            model_name: None,
            temperature: None,
            workspace_dir: None,
            identity_config: None,
            skills: None,
            auto_save: None,
            classification_config: None,
            task_routing_config: None,
            available_hints: None,
            #[cfg(feature = "llm-router")]
            router: None,
        }
    }

    pub fn provider(mut self, provider: Box<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    pub fn tools(mut self, tools: Vec<Box<dyn Tool>>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn memory(mut self, memory: Arc<dyn Memory>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    pub fn hooks(mut self, hooks: Arc<HookManager>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    pub fn prompt_builder(mut self, prompt_builder: SystemPromptBuilder) -> Self {
        self.prompt_builder = Some(prompt_builder);
        self
    }

    pub fn tool_dispatcher(mut self, tool_dispatcher: Box<dyn ToolDispatcher>) -> Self {
        self.tool_dispatcher = Some(tool_dispatcher);
        self
    }

    pub fn memory_loader(mut self, memory_loader: Box<dyn MemoryLoader>) -> Self {
        self.memory_loader = Some(memory_loader);
        self
    }

    pub fn config(mut self, config: crate::config::AgentConfig) -> Self {
        self.config = Some(config);
        self
    }

    pub fn model_name(mut self, model_name: String) -> Self {
        self.model_name = Some(model_name);
        self
    }

    pub fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn workspace_dir(mut self, workspace_dir: std::path::PathBuf) -> Self {
        self.workspace_dir = Some(workspace_dir);
        self
    }

    pub fn identity_config(mut self, identity_config: crate::config::IdentityConfig) -> Self {
        self.identity_config = Some(identity_config);
        self
    }

    pub fn skills(mut self, skills: Vec<crate::skills::Skill>) -> Self {
        self.skills = Some(skills);
        self
    }

    pub fn auto_save(mut self, auto_save: bool) -> Self {
        self.auto_save = Some(auto_save);
        self
    }

    pub fn classification_config(
        mut self,
        classification_config: crate::config::QueryClassificationConfig,
    ) -> Self {
        self.classification_config = Some(classification_config);
        self
    }

    pub fn available_hints(mut self, available_hints: Vec<String>) -> Self {
        self.available_hints = Some(available_hints);
        self
    }

    pub fn task_routing_config(
        mut self,
        task_routing_config: crate::config::TaskRoutingConfig,
    ) -> Self {
        self.task_routing_config = Some(task_routing_config);
        self
    }

    #[cfg(feature = "llm-router")]
    pub fn router(mut self, router: RouterEngine) -> Self {
        self.router = Some(router);
        self
    }

    pub fn build(self) -> Result<Agent> {
        let tools = self
            .tools
            .ok_or_else(|| anyhow::anyhow!("tools are required"))?;
        let workspace_dir = self
            .workspace_dir
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let hooks = self
            .hooks
            .unwrap_or_else(|| Arc::new(HookManager::new(workspace_dir.clone())));

        Ok(Agent {
            provider: self
                .provider
                .ok_or_else(|| anyhow::anyhow!("provider is required"))?,
            tools,
            memory: self
                .memory
                .ok_or_else(|| anyhow::anyhow!("memory is required"))?,
            observer: self
                .observer
                .ok_or_else(|| anyhow::anyhow!("observer is required"))?,
            hooks,
            prompt_builder: self
                .prompt_builder
                .unwrap_or_else(SystemPromptBuilder::with_defaults),
            tool_dispatcher: self
                .tool_dispatcher
                .ok_or_else(|| anyhow::anyhow!("tool_dispatcher is required"))?,
            memory_loader: self
                .memory_loader
                .unwrap_or_else(|| Box::new(DefaultMemoryLoader::default())),
            config: self.config.unwrap_or_default(),
            model_name: self
                .model_name
                .unwrap_or_else(|| "anthropic/claude-sonnet-4-20250514".into()),
            temperature: self.temperature.unwrap_or(0.7),
            workspace_dir,
            identity_config: self.identity_config.unwrap_or_default(),
            skills: self.skills.unwrap_or_default(),
            auto_save: self.auto_save.unwrap_or(false),
            history: Vec::new(),
            classification_config: self.classification_config.unwrap_or_default(),
            task_routing_config: self.task_routing_config.unwrap_or_default(),
            available_hints: self.available_hints.unwrap_or_default(),
            #[cfg(feature = "llm-router")]
            router: self.router,
        })
    }
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    pub fn history(&self) -> &[ConversationMessage] {
        &self.history
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    pub fn from_config(config: &Config) -> Result<Self> {
        let observer: Arc<dyn Observer> =
            Arc::from(observability::create_observer(&config.observability));
        let runtime: Arc<dyn runtime::RuntimeAdapter> =
            Arc::from(runtime::create_runtime(&config.runtime)?);
        let security = Arc::new(SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ));

        let memory: Arc<dyn Memory> =
            Arc::from(memory::create_memory_with_storage_and_routes_with_acl(
                &config.memory,
                &config.embedding_routes,
                Some(&config.storage.provider.config),
                &config.workspace_dir,
                config.api_key.as_deref(),
                &config.identity_bindings,
                &config.user_policies,
            )?);

        let composio_key = if config.composio.enabled {
            config.composio.api_key.as_deref()
        } else {
            None
        };
        let composio_entity_id = if config.composio.enabled {
            Some(config.composio.entity_id.as_str())
        } else {
            None
        };

        let tools = tools::all_tools_with_runtime(
            Arc::new(config.clone()),
            &security,
            runtime,
            memory.clone(),
            composio_key,
            composio_entity_id,
            &config.browser,
            &config.http_request,
            &config.workspace_dir,
            &config.agents,
            config.api_key.as_deref(),
            config,
        );

        let provider_name = config.default_provider.as_deref().unwrap_or("openrouter");

        let model_name = config
            .default_model
            .as_deref()
            .unwrap_or("anthropic/claude-sonnet-4-20250514")
            .to_string();

        let provider: Box<dyn Provider> = providers::create_routed_provider(
            provider_name,
            config.api_key.as_deref(),
            config.api_url.as_deref(),
            &config.reliability,
            &config.model_routes,
            &model_name,
        )?;

        let dispatcher_choice = config.agent.tool_dispatcher.as_str();
        let tool_dispatcher: Box<dyn ToolDispatcher> = match dispatcher_choice {
            "native" => Box::new(NativeToolDispatcher),
            "xml" => Box::new(XmlToolDispatcher),
            _ if provider.supports_native_tools() => Box::new(NativeToolDispatcher),
            _ => Box::new(XmlToolDispatcher),
        };

        let available_hints: Vec<String> =
            config.model_routes.iter().map(|r| r.hint.clone()).collect();

        #[cfg(feature = "llm-router")]
        let mut builder = Agent::builder()
            .provider(provider)
            .tools(tools)
            .memory(memory)
            .observer(observer)
            .hooks(Arc::new(HookManager::new(config.workspace_dir.clone())))
            .tool_dispatcher(tool_dispatcher)
            .memory_loader(Box::new(DefaultMemoryLoader::new(
                5,
                config.memory.min_relevance_score,
            )))
            .prompt_builder(SystemPromptBuilder::with_defaults())
            .config(config.agent.clone())
            .model_name(model_name)
            .temperature(config.default_temperature)
            .workspace_dir(config.workspace_dir.clone())
            .classification_config(config.query_classification.clone())
            .task_routing_config(config.task_routing.clone())
            .available_hints(available_hints.clone())
            .identity_config(config.identity.clone())
            .skills(crate::skills::load_skills_with_config(
                &config.workspace_dir,
                config,
            ))
            .auto_save(config.memory.auto_save);

        #[cfg(not(feature = "llm-router"))]
        let builder = Agent::builder()
            .provider(provider)
            .tools(tools)
            .memory(memory)
            .observer(observer)
            .hooks(Arc::new(HookManager::new(config.workspace_dir.clone())))
            .tool_dispatcher(tool_dispatcher)
            .memory_loader(Box::new(DefaultMemoryLoader::new(
                5,
                config.memory.min_relevance_score,
            )))
            .prompt_builder(SystemPromptBuilder::with_defaults())
            .config(config.agent.clone())
            .model_name(model_name)
            .temperature(config.default_temperature)
            .workspace_dir(config.workspace_dir.clone())
            .classification_config(config.query_classification.clone())
            .task_routing_config(config.task_routing.clone())
            .available_hints(available_hints)
            .identity_config(config.identity.clone())
            .skills(crate::skills::load_skills_with_config(
                &config.workspace_dir,
                config,
            ))
            .auto_save(config.memory.auto_save);

        #[cfg(feature = "llm-router")]
        if config.router.enabled {
            let router_embedder =
                memory::create_embedder_from_config(config, config.api_key.as_deref());
            let router = futures::executor::block_on(RouterEngine::new(
                config.router.clone(),
                builder.memory.as_ref().expect("memory set").clone(),
                Some(router_embedder),
            ))?;
            builder = builder.router(router);
        }

        builder.build()
    }

    fn trim_history(&mut self) {
        let max = self.config.max_history_messages;
        if self.history.len() <= max {
            return;
        }

        let mut system_messages = Vec::new();
        let mut other_messages = Vec::new();

        for msg in self.history.drain(..) {
            match &msg {
                ConversationMessage::Chat(chat) if chat.role == "system" => {
                    system_messages.push(msg);
                }
                _ => other_messages.push(msg),
            }
        }

        if other_messages.len() > max {
            let drop_count = other_messages.len() - max;
            other_messages.drain(0..drop_count);
        }

        self.history = system_messages;
        self.history.extend(other_messages);
    }

    fn build_system_prompt(&self) -> Result<String> {
        let instructions = self.tool_dispatcher.prompt_instructions(&self.tools);
        let ctx = PromptContext {
            workspace_dir: &self.workspace_dir,
            model_name: &self.model_name,
            tools: &self.tools,
            skills: &self.skills,
            identity_config: Some(&self.identity_config),
            dispatcher_instructions: &instructions,
        };
        self.prompt_builder.build(&ctx)
    }

    async fn execute_tool_call(&self, call: &ParsedToolCall) -> ToolExecutionResult {
        let start = Instant::now();
        self.hooks
            .emit(
                HookEvent::ToolCallStart,
                serde_json::json!({
                    "tool": call.name,
                    "arguments": call.arguments,
                }),
            )
            .await;

        let result = if let Some(tool) = self.tools.iter().find(|t| t.supports_name(&call.name)) {
            match tool.execute_named(&call.name, call.arguments.clone()).await {
                Ok(r) => {
                    self.hooks
                        .emit(
                            HookEvent::ToolCall,
                            serde_json::json!({
                                "tool": call.name,
                                "duration_ms": start.elapsed().as_millis(),
                                "success": r.success,
                                "error": r.error,
                                "output": r.output,
                            }),
                        )
                        .await;
                    self.observer.record_event(&ObserverEvent::ToolCall {
                        tool: call.name.clone(),
                        duration: start.elapsed(),
                        success: r.success,
                    });
                    if r.success {
                        r.output
                    } else {
                        format!("Error: {}", r.error.unwrap_or(r.output))
                    }
                }
                Err(e) => {
                    let message = format!("Error executing {}: {e}", call.name);
                    self.hooks
                        .emit(HookEvent::Error, payload_error("tool", &message))
                        .await;
                    self.observer.record_event(&ObserverEvent::ToolCall {
                        tool: call.name.clone(),
                        duration: start.elapsed(),
                        success: false,
                    });
                    message
                }
            }
        } else {
            let message = format!("Unknown tool: {}", call.name);
            self.hooks
                .emit(HookEvent::Error, payload_error("tool", &message))
                .await;
            message
        };

        ToolExecutionResult {
            name: call.name.clone(),
            output: result,
            success: true,
            tool_call_id: call.tool_call_id.clone(),
        }
    }

    async fn execute_tools(&self, calls: &[ParsedToolCall]) -> Vec<ToolExecutionResult> {
        if !self.config.parallel_tools {
            let mut results = Vec::with_capacity(calls.len());
            for call in calls {
                results.push(self.execute_tool_call(call).await);
            }
            return results;
        }

        let futs: Vec<_> = calls
            .iter()
            .map(|call| self.execute_tool_call(call))
            .collect();
        futures::future::join_all(futs).await
    }

    fn classify_model(&self, user_message: &str) -> String {
        if let Some(hint) = super::classifier::classify(&self.classification_config, user_message) {
            return self.resolve_model_target(&hint);
        }
        self.model_name.clone()
    }

    fn resolve_model_target(&self, target: &str) -> String {
        if self.available_hints.contains(&target.to_string()) {
            tracing::info!(hint = target, "Auto-classified query");
            return format!("hint:{target}");
        }

        target.to_string()
    }

    #[cfg(feature = "llm-router")]
    fn resolve_router_target(&self, provider: &str, model: &str) -> String {
        if provider == "ollama" && model == "*" {
            if let Some(default_model) = self
                .model_name
                .strip_prefix("ollama/")
                .filter(|value| !value.is_empty())
            {
                return format!("{provider}/{default_model}");
            }
        }

        format!("{provider}/{model}")
    }

    #[cfg(feature = "llm-router")]
    fn estimate_text_tokens(text: &str) -> usize {
        text.chars().count() / 4 + 1
    }

    #[cfg(feature = "llm-router")]
    async fn append_router_cost_event(
        &self,
        event: &RouterCostEvent,
    ) {
        let date = Utc::now().format("%Y-%m-%d").to_string();
        let key = format!("router/cost/{date}");
        let mut events = self
            .memory
            .get(&key)
            .await
            .ok()
            .flatten()
            .and_then(|entry| serde_json::from_str::<Vec<RouterCostEvent>>(&entry.content).ok())
            .unwrap_or_default();
        events.push(event.clone());

        if let Ok(payload) = serde_json::to_string(&events) {
            if let Err(err) = self
                .memory
                .store(
                    &key,
                    &payload,
                    MemoryCategory::Custom("router".to_string()),
                    Some(SELF_SYSTEM_SESSION_ID),
                )
                .await
            {
                tracing::warn!("Automix cost tracking store failed: {err}");
            }
        }
    }

    async fn spawn_delegate_task(
        &self,
        user_message: &str,
        sub_agent_model: Option<&str>,
    ) -> Result<String> {
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.supports_name("sessions_spawn"))
            .ok_or_else(|| anyhow::anyhow!("sessions_spawn tool is not registered"))?;

        let mut args = serde_json::json!({
            "action": "spawn",
            "task": user_message,
            "mode": "process",
        });

        if let Some(model) = sub_agent_model
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            args["model"] = serde_json::Value::String(model.to_string());
        }

        let result = tool.execute_named("sessions_spawn", args).await?;
        if !result.success {
            anyhow::bail!(
                "{}",
                result
                    .error
                    .unwrap_or_else(|| "sessions_spawn delegation failed".to_string())
            );
        }

        // Prefer structured run_id from JSON output; fall back to text parsing
        // only if the tool does not return a machine-readable envelope.
        let run_id: Option<String> =
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&result.output) {
                json.get("run_id")
                    .or_else(|| json.get("id"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            } else {
                None
            };

        let run_id = run_id.or_else(|| {
            // Legacy text parsing — emit a warning so future regressions surface.
            // Log only a bounded, sanitized summary to avoid leaking task content.
            let preview: String = result.output.chars().take(80).collect();
            tracing::warn!(
                output_preview = preview.as_str(),
                output_len = result.output.len(),
                "spawn_delegate_task: run_id not found in structured output; \
                 falling back to text parsing. Consider returning structured metadata \
                 from sessions_spawn."
            );
            result
                .output
                .split("(run_id: ")
                .nth(1)
                .and_then(|rest| rest.split(')').next())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        });

        // Return a generic error — do not embed raw tool output which may
        // contain sensitive task content or internal details.
        run_id.ok_or_else(|| {
            anyhow::anyhow!(
                "spawn_delegate_task: sessions_spawn succeeded but returned no parseable \
                 run_id. Check sessions_spawn output format."
            )
        })
    }

    pub async fn turn(&mut self, user_message: &str) -> Result<String> {
        #[cfg(feature = "llm-router")]
        let turn_start = Instant::now();
        if self.history.is_empty() {
            let system_prompt = self.build_system_prompt()?;
            self.history
                .push(ConversationMessage::Chat(ChatMessage::system(
                    system_prompt,
                )));
        }

        if self.auto_save && memory::should_autosave_content(user_message) {
            let _ = self
                .memory
                .store("user_msg", user_message, MemoryCategory::Conversation, None)
                .await;
        }

        let context = self
            .memory_loader
            .load_context(self.memory.as_ref(), user_message)
            .await
            .unwrap_or_default();

        let enriched = if context.is_empty() {
            user_message.to_string()
        } else {
            format!("{context}{user_message}")
        };

        let classify_result =
            super::classifier::classify_intent(&self.task_routing_config, user_message);
        tracing::info!(
            intent = ?classify_result.intent,
            reason = classify_result.reason.as_str(),
            "Task routing classified incoming message"
        );

        if classify_result.intent == super::classifier::TaskIntent::Delegate {
            // Record user message in history before delegating so follow-up
            // turns can see that delegation happened.
            self.history
                .push(ConversationMessage::Chat(ChatMessage::user(
                    enriched.clone(),
                )));
            let task_id = self
                .spawn_delegate_task(user_message, classify_result.model_hint.as_deref())
                .await?;
            let ack = format!("已收到，正在后台处理（任务 {task_id}），完成后会回传结果。");
            // Record acknowledgment in history as well.
            self.history
                .push(ConversationMessage::Chat(ChatMessage::assistant(
                    ack.clone(),
                )));
            return Ok(ack);
        }

        self.history
            .push(ConversationMessage::Chat(ChatMessage::user(enriched)));

        #[allow(unused_mut)]
        let mut effective_model = {
            #[cfg(feature = "llm-router")]
            {
                if let Some(router) = &self.router {
                    if classify_result.model_hint.is_none() {
                        let result = router
                            .select_model(user_message, &classify_result.intent)
                            .await;
                        if let (Some(provider), Some(model)) = (
                            result.chosen_provider.as_deref(),
                            result.chosen_model.as_deref(),
                        ) {
                            tracing::info!(
                                chosen = model,
                                provider = provider,
                                score = result.score,
                                "Router selected model"
                            );
                            self.resolve_router_target(provider, model)
                        } else {
                            self.classify_model(user_message)
                        }
                    } else {
                        classify_result
                            .model_hint
                            .as_deref()
                            .map(|target| self.resolve_model_target(target))
                            .unwrap()
                    }
                } else {
                    classify_result
                        .model_hint
                        .as_deref()
                        .map(|target| self.resolve_model_target(target))
                        .unwrap_or_else(|| self.classify_model(user_message))
                }
            }
            #[cfg(not(feature = "llm-router"))]
            {
                classify_result
                    .model_hint
                    .as_deref()
                    .map(|target| self.resolve_model_target(target))
                    .unwrap_or_else(|| self.classify_model(user_message))
            }
        };
        #[cfg(feature = "llm-router")]
        let mut cost_event = RouterCostEvent::new(&effective_model);
        let max_tool_iterations = match classify_result.intent {
            // Simple: cap at configured limit but allow up to 5 — not a hard 3.
            // The previous hard-cap of 3 silently ignored higher configured values.
            super::classifier::TaskIntent::Simple => self.config.max_tool_iterations.clamp(1, 5),
            super::classifier::TaskIntent::Stream => self.config.max_tool_iterations.max(1),
            super::classifier::TaskIntent::Delegate => unreachable!(),
        };

        for _ in 0..max_tool_iterations {
            let messages = self.tool_dispatcher.to_provider_messages(&self.history);
            for tool in &self.tools {
                let _ = tool.refresh().await;
            }
            self.hooks
                .emit(
                    HookEvent::LlmRequest,
                    serde_json::json!({
                        "provider": "configured",
                        "model": self.model_name,
                        "messages_count": messages.len(),
                    }),
                )
                .await;
            let dynamic_tool_specs = self
                .tools
                .iter()
                .flat_map(|tool| tool.specs())
                .collect::<Vec<_>>();
            #[allow(unused_mut)]
            let mut response = match self
                .provider
                .chat(
                    ChatRequest {
                        messages: &messages,
                        tools: if self.tool_dispatcher.should_send_tool_specs() {
                            Some(&dynamic_tool_specs)
                        } else {
                            None
                        },
                    },
                    &effective_model,
                    self.temperature,
                )
                .await
            {
                Ok(resp) => resp,
                Err(err) => {
                    #[cfg(feature = "llm-router")]
                    if let Some(router) = &self.router {
                        cost_event.primary_prompt_tokens += Self::estimate_text_tokens(user_message);
                        let success = false;
                        let latency = turn_start.elapsed().as_millis() as u64;
                        if let Err(record_err) = router
                            .record_outcome(user_message, &effective_model, success, latency)
                            .await
                        {
                            tracing::warn!("Router record_outcome failed: {record_err}");
                        }
                        cost_event.primary_model = effective_model.clone();
                        cost_event.total_cost_usd = router
                            .model_cost_per_million_tokens(&effective_model)
                            .map(|rate| {
                                rate * cost_event.primary_prompt_tokens as f32 / 1_000_000.0
                            })
                            .unwrap_or(0.0);
                        self.append_router_cost_event(&cost_event).await;
                    }
                    self.hooks
                        .emit(HookEvent::Error, payload_error("llm", &err.to_string()))
                        .await;
                    return Err(err);
                }
            };
            self.hooks
                .emit(
                    HookEvent::LlmResponse,
                    serde_json::json!({
                        "provider": "configured",
                        "model": self.model_name,
                        "success": true,
                    }),
                )
                .await;

            #[allow(unused_mut)]
            let mut parsed = self.tool_dispatcher.parse_response(&response);
            #[cfg(feature = "llm-router")]
            if parsed.1.is_empty() {
                if let Some(router) = &self.router {
                    if let Some(automix) = router.automix_config() {
                        let initial_text = if parsed.0.is_empty() {
                            response.text.clone().unwrap_or_default()
                        } else {
                            parsed.0.clone()
                        };
                        cost_event.primary_model = effective_model.clone();
                        cost_event.primary_prompt_tokens += Self::estimate_text_tokens(user_message);
                        cost_event.primary_completion_tokens +=
                            Self::estimate_text_tokens(&initial_text);

                        if automix.enabled
                            && !automix.premium_model_id.trim().is_empty()
                            && is_cheap_model_target(&effective_model, &automix.cheap_model_tiers)
                        {
                            let confidence =
                                ConfidenceChecker::check_rules(&initial_text, user_message);
                            cost_event.confidence = confidence;

                            if should_escalate(confidence, automix.confidence_threshold) {
                                let premium_model =
                                    self.resolve_model_target(&automix.premium_model_id);
                                tracing::info!(
                                    confidence,
                                    escalate_to = premium_model.as_str(),
                                    "Automix: escalating to premium model"
                                );
                                let premium_response = match self
                                    .provider
                                    .chat(
                                        ChatRequest {
                                            messages: &messages,
                                            tools: if self.tool_dispatcher.should_send_tool_specs() {
                                                Some(&dynamic_tool_specs)
                                            } else {
                                                None
                                            },
                                        },
                                        &premium_model,
                                        self.temperature,
                                    )
                                    .await
                                {
                                    Ok(resp) => resp,
                                    Err(err) => {
                                        tracing::warn!(
                                            model = premium_model.as_str(),
                                            "Automix premium escalation failed: {err}"
                                        );
                                        response
                                    }
                                };
                                cost_event.escalated = true;
                                cost_event.escalation_model = Some(premium_model.clone());
                                cost_event.escalation_prompt_tokens +=
                                    Self::estimate_text_tokens(user_message);
                                let premium_text =
                                    premium_response.text.clone().unwrap_or_default();
                                cost_event.escalation_completion_tokens +=
                                    Self::estimate_text_tokens(&premium_text);
                                effective_model = premium_model;
                                response = premium_response;
                                parsed = self.tool_dispatcher.parse_response(&response);
                            } else {
                                tracing::info!(
                                    confidence,
                                    threshold = automix.confidence_threshold,
                                    "Automix: confidence sufficient"
                                );
                            }
                        }
                    }
                }
            }

            let (text, calls) = parsed;
            if calls.is_empty() {
                let final_text = if text.is_empty() {
                    response.text.unwrap_or_default()
                } else {
                    text
                };

                self.history
                    .push(ConversationMessage::Chat(ChatMessage::assistant(
                        final_text.clone(),
                    )));
                self.trim_history();

                // Note: assistant responses are intentionally NOT auto-saved here.
                // Only user messages are persisted via auto_save to avoid storing
                // AI-generated content as if it were factual user input.
                self.hooks
                    .emit(
                        HookEvent::TurnComplete,
                        serde_json::json!({
                            "mode": "agent",
                            "response_chars": final_text.chars().count(),
                        }),
                    )
                    .await;
                #[cfg(feature = "llm-router")]
                if let Some(router) = &self.router {
                    let success = !final_text.is_empty();
                    let latency = turn_start.elapsed().as_millis() as u64;
                    if let Err(err) = router
                        .record_outcome(user_message, &effective_model, success, latency)
                        .await
                    {
                        tracing::warn!("Router record_outcome failed: {err}");
                    }
                    cost_event.total_cost_usd = router
                        .model_cost_per_million_tokens(&cost_event.primary_model)
                        .map(|rate| {
                            rate * (cost_event.primary_prompt_tokens + cost_event.primary_completion_tokens)
                                as f32
                                / 1_000_000.0
                        })
                        .unwrap_or(0.0);
                    if let Some(model) = &cost_event.escalation_model {
                        if let Some(rate) = router.model_cost_per_million_tokens(model) {
                            cost_event.total_cost_usd += rate
                                * (cost_event.escalation_prompt_tokens
                                    + cost_event.escalation_completion_tokens)
                                    as f32
                                / 1_000_000.0;
                        }
                    }
                }
                #[cfg(feature = "llm-router")]
                self.append_router_cost_event(&cost_event).await;
                return Ok(final_text);
            }

            if !text.is_empty() {
                self.history
                    .push(ConversationMessage::Chat(ChatMessage::assistant(
                        text.clone(),
                    )));
                print!("{text}");
                let _ = std::io::stdout().flush();
            }

            self.history.push(ConversationMessage::AssistantToolCalls {
                text: response.text.clone(),
                tool_calls: response.tool_calls.clone(),
            });

            let results = self.execute_tools(&calls).await;
            let formatted = self.tool_dispatcher.format_results(&results);
            self.history.push(formatted);
            self.trim_history();
        }

        #[cfg(feature = "llm-router")]
        if let Some(router) = &self.router {
            let success = false;
            let latency = turn_start.elapsed().as_millis() as u64;
            if let Err(err) = router
                .record_outcome(user_message, &effective_model, success, latency)
                .await
            {
                tracing::warn!("Router record_outcome failed: {err}");
            }
            self.append_router_cost_event(&cost_event).await;
        }
        anyhow::bail!(
            "Agent exceeded maximum tool iterations ({})",
            max_tool_iterations
        )
    }

    pub async fn run_single(&mut self, message: &str) -> Result<String> {
        self.turn(message).await
    }

    pub async fn run_interactive(&mut self) -> Result<()> {
        println!("🦀 OpenPRX Interactive Mode");
        println!("Type /quit to exit.\n");

        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let cli = crate::channels::CliChannel::new();

        let listen_handle = tokio::spawn(async move {
            let _ = crate::channels::Channel::listen(&cli, tx).await;
        });

        while let Some(msg) = rx.recv().await {
            let response = match self.turn(&msg.content).await {
                Ok(resp) => resp,
                Err(e) => {
                    eprintln!("\nError: {e}\n");
                    continue;
                }
            };
            println!("\n{response}\n");
        }

        listen_handle.abort();
        Ok(())
    }
}

#[cfg(feature = "llm-router")]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct RouterCostEvent {
    primary_model: String,
    escalated: bool,
    escalation_model: Option<String>,
    confidence: f32,
    primary_prompt_tokens: usize,
    primary_completion_tokens: usize,
    escalation_prompt_tokens: usize,
    escalation_completion_tokens: usize,
    total_cost_usd: f32,
}

#[cfg(feature = "llm-router")]
impl RouterCostEvent {
    fn new(primary_model: &str) -> Self {
        Self {
            primary_model: primary_model.to_string(),
            escalated: false,
            escalation_model: None,
            confidence: 1.0,
            primary_prompt_tokens: 0,
            primary_completion_tokens: 0,
            escalation_prompt_tokens: 0,
            escalation_completion_tokens: 0,
            total_cost_usd: 0.0,
        }
    }
}

pub async fn run(
    config: Config,
    message: Option<String>,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
) -> Result<()> {
    let start = Instant::now();

    let mut effective_config = config;
    if let Some(p) = provider_override {
        effective_config.default_provider = Some(p);
    }
    if let Some(m) = model_override {
        effective_config.default_model = Some(m);
    }
    effective_config.default_temperature = temperature;

    let mut agent = Agent::from_config(&effective_config)?;

    let provider_name = effective_config
        .default_provider
        .as_deref()
        .unwrap_or("openrouter")
        .to_string();
    let model_name = effective_config
        .default_model
        .as_deref()
        .unwrap_or("anthropic/claude-sonnet-4-20250514")
        .to_string();

    agent.observer.record_event(&ObserverEvent::AgentStart {
        provider: provider_name.clone(),
        model: model_name.clone(),
    });
    agent
        .hooks
        .emit(
            HookEvent::AgentStart,
            serde_json::json!({
                "provider": effective_config.default_provider,
                "model": effective_config.default_model,
            }),
        )
        .await;

    if let Some(msg) = message {
        let response = agent.run_single(&msg).await?;
        println!("{response}");
    } else {
        agent.run_interactive().await?;
    }

    agent.observer.record_event(&ObserverEvent::AgentEnd {
        provider: provider_name,
        model: model_name,
        duration: start.elapsed(),
        tokens_used: None,
        cost_usd: None,
    });
    agent
        .hooks
        .emit(
            HookEvent::AgentEnd,
            serde_json::json!({
                "duration_ms": start.elapsed().as_millis(),
            }),
        )
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    #[cfg(feature = "llm-router")]
    use std::collections::HashMap;
    #[cfg(feature = "llm-router")]
    use std::sync::Arc;

    struct MockProvider {
        responses: Mutex<Vec<crate::providers::ChatResponse>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> Result<String> {
            Ok("ok".into())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> Result<crate::providers::ChatResponse> {
            let mut guard = self.responses.lock();
            if guard.is_empty() {
                return Ok(crate::providers::ChatResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                });
            }
            Ok(guard.remove(0))
        }
    }

    struct MockTool;

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "echo"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _args: serde_json::Value) -> Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: "tool-out".into(),
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn turn_without_tools_returns_text() {
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![crate::providers::ChatResponse {
                text: Some("hello".into()),
                tool_calls: vec![],
            }]),
        });

        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory creation should succeed with valid config"),
        );

        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder should succeed with valid config");

        let response = agent.turn("hi").await.unwrap();
        assert_eq!(response, "hello");
    }

    #[tokio::test]
    async fn turn_with_native_dispatcher_handles_tool_results_variant() {
        let provider = Box::new(MockProvider {
            responses: Mutex::new(vec![
                crate::providers::ChatResponse {
                    text: Some(String::new()),
                    tool_calls: vec![crate::providers::ToolCall {
                        id: "tc1".into(),
                        name: "echo".into(),
                        arguments: "{}".into(),
                    }],
                },
                crate::providers::ChatResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                },
            ]),
        });

        let memory_cfg = crate::config::MemoryConfig {
            backend: "none".into(),
            ..crate::config::MemoryConfig::default()
        };
        let mem: Arc<dyn Memory> = Arc::from(
            crate::memory::create_memory(&memory_cfg, std::path::Path::new("/tmp"), None)
                .expect("memory creation should succeed with valid config"),
        );

        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let mut agent = Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(mem)
            .observer(observer)
            .tool_dispatcher(Box::new(NativeToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder should succeed with valid config");

        let response = agent.turn("hi").await.unwrap();
        assert_eq!(response, "done");
        assert!(agent
            .history()
            .iter()
            .any(|msg| matches!(msg, ConversationMessage::ToolResults(_))));
    }

    #[cfg(feature = "llm-router")]
    #[derive(Default)]
    struct TestMemory {
        entries: Mutex<HashMap<String, crate::memory::MemoryEntry>>,
    }

    #[cfg(feature = "llm-router")]
    #[async_trait]
    impl Memory for TestMemory {
        fn name(&self) -> &str {
            "test"
        }

        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> Result<()> {
            self.entries.lock().insert(
                key.to_string(),
                crate::memory::MemoryEntry {
                    id: key.to_string(),
                    key: key.to_string(),
                    content: content.to_string(),
                    category,
                    timestamp: "2026-03-10T00:00:00Z".into(),
                    session_id: session_id.map(str::to_string),
                    score: None,
                    tags: None,
                    access_count: None,
                    useful_count: None,
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                },
            );
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> Result<Vec<crate::memory::MemoryEntry>> {
            Ok(Vec::new())
        }

        async fn get(&self, key: &str) -> Result<Option<crate::memory::MemoryEntry>> {
            Ok(self.entries.lock().get(key).cloned())
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> Result<Vec<crate::memory::MemoryEntry>> {
            Ok(self.entries.lock().values().cloned().collect())
        }

        async fn forget(&self, key: &str) -> Result<bool> {
            Ok(self.entries.lock().remove(key).is_some())
        }

        async fn count(&self) -> Result<usize> {
            Ok(self.entries.lock().len())
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    #[cfg(feature = "llm-router")]
    struct RecordingProvider {
        models: Arc<Mutex<Vec<String>>>,
        cheap_response: String,
        premium_response: String,
    }

    #[cfg(feature = "llm-router")]
    #[async_trait]
    impl Provider for RecordingProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> Result<String> {
            Ok("ok".into())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            model: &str,
            _temperature: f64,
        ) -> Result<crate::providers::ChatResponse> {
            self.models.lock().push(model.to_string());
            let text = if model.contains("premium") {
                self.premium_response.clone()
            } else {
                self.cheap_response.clone()
            };
            Ok(crate::providers::ChatResponse {
                text: Some(text),
                tool_calls: vec![],
            })
        }
    }

    #[cfg(feature = "llm-router")]
    fn automix_router_config(enabled: bool) -> crate::config::RouterConfig {
        crate::config::RouterConfig {
            enabled: true,
            alpha: 0.0,
            beta: 0.5,
            gamma: 0.3,
            delta: 1.0,
            epsilon: 0.3,
            knn_enabled: false,
            knn_min_records: 10,
            knn_k: 7,
            automix: crate::config::AutomixConfig {
                enabled,
                confidence_threshold: 0.7,
                cheap_model_tiers: vec!["mini".into(), "ollama".into()],
                premium_model_id: "openai/model-premium".into(),
            },
            models: vec![
                crate::config::RouterModelConfig {
                    model_id: "model-mini".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 0.1,
                    max_context: 128_000,
                    latency_ms: 500,
                    categories: vec!["conversation".into()],
                    elo_rating: 1_000.0,
                },
                crate::config::RouterModelConfig {
                    model_id: "model-premium".into(),
                    provider: "openai".into(),
                    cost_per_million_tokens: 10.0,
                    max_context: 128_000,
                    latency_ms: 2_000,
                    categories: vec!["conversation".into()],
                    elo_rating: 1_000.0,
                },
            ],
        }
    }

    #[cfg(feature = "llm-router")]
    async fn build_automix_agent(
        cheap_response: &str,
        premium_response: &str,
        automix_enabled: bool,
    ) -> (Agent, Arc<TestMemory>, Arc<Mutex<Vec<String>>>) {
        let memory = Arc::new(TestMemory::default());
        let observer: Arc<dyn Observer> = Arc::from(crate::observability::NoopObserver {});
        let models = Arc::new(Mutex::new(Vec::new()));
        let provider = Box::new(RecordingProvider {
            models: Arc::clone(&models),
            cheap_response: cheap_response.into(),
            premium_response: premium_response.into(),
        });
        let router = RouterEngine::new(automix_router_config(automix_enabled), memory.clone(), None)
            .await
            .unwrap();

        let agent = Agent::builder()
            .provider(provider)
            .tools(vec![Box::new(MockTool)])
            .memory(memory.clone())
            .observer(observer)
            .tool_dispatcher(Box::new(XmlToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .router(router)
            .build()
            .unwrap();

        (agent, memory, models)
    }

    #[cfg(feature = "llm-router")]
    #[tokio::test]
    async fn test_automix_escalates_on_low_confidence() {
        let (mut agent, _memory, models) = build_automix_agent(
            "I'm not sure, maybe this is the answer.",
            "This is the premium answer.",
            true,
        )
        .await;

        let response = agent.turn("hello").await.unwrap();

        assert_eq!(response, "This is the premium answer.");
        assert_eq!(
            models.lock().clone(),
            vec!["ollama/*".to_string(), "openai/model-premium".to_string()]
        );
    }

    #[cfg(feature = "llm-router")]
    #[tokio::test]
    async fn test_automix_skips_escalation_on_high_confidence() {
        let (mut agent, _memory, models) = build_automix_agent(
            "```rust\nfn main() {}\n```\nThis compiles cleanly.",
            "This should not be used.",
            true,
        )
        .await;

        let response = agent.turn("fix this rust code").await.unwrap();

        assert!(response.contains("compiles cleanly"));
        assert_eq!(models.lock().clone(), vec!["ollama/*".to_string()]);
    }

    #[cfg(feature = "llm-router")]
    #[tokio::test]
    async fn test_automix_disabled() {
        let (mut agent, _memory, models) = build_automix_agent(
            "I'm not sure, maybe this is the answer.",
            "This should not be used.",
            false,
        )
        .await;

        let response = agent.turn("hello").await.unwrap();

        assert!(response.contains("not sure"));
        assert_eq!(models.lock().clone(), vec!["ollama/*".to_string()]);
    }

    #[cfg(feature = "llm-router")]
    #[tokio::test]
    async fn test_cost_tracking() {
        let (mut agent, memory, _models) = build_automix_agent(
            "I'm not sure, maybe this is the answer.",
            "This is the premium answer.",
            true,
        )
        .await;

        let _ = agent.turn("hello").await.unwrap();

        let key = format!("router/cost/{}", chrono::Utc::now().format("%Y-%m-%d"));
        let entry = memory.get(&key).await.unwrap().expect("cost entry");
        let events: Vec<RouterCostEvent> = serde_json::from_str(&entry.content).unwrap();

        assert_eq!(entry.session_id.as_deref(), Some(crate::self_system::SELF_SYSTEM_SESSION_ID));
        assert!(!events.is_empty());
        assert!(events.last().unwrap().escalated);
        assert!(events.last().unwrap().total_cost_usd > 0.0);
    }
}
