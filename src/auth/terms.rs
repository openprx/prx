//! Operator-facing explanations for login methods PRX no longer performs.

pub const ANTHROPIC_SUBSCRIPTION_REMOVED: &str = "\
Anthropic does not allow third-party apps to use Claude.ai subscription credentials \
(Free, Pro, Max, Ultra, or Team), including setup-tokens (sk-ant-oat01) and Claude Code \
OAuth in ~/.claude/.credentials.json. Use an Anthropic Console API key with provider \
\"anthropic\". Max, Ultra, and Team plans include monthly API credits: \
https://support.claude.com/en/articles/17154008 . \
Stored credentials were not deleted. Remove a PRX profile with \
`prx auth logout --provider anthropic`.";

pub const GEMINI_CLI_OAUTH_REMOVED: &str = "\
Reusing Gemini CLI OAuth (~/.gemini/oauth_creds.json and cloudcode-pa) is not permitted. \
Use a Gemini API key (GEMINI_API_KEY or provider \"gemini\"). The CLI credential file was \
not read or modified.";

pub const QWEN_OAUTH_REMOVED: &str = "\
Qwen Code OAuth (the free tier and ~/.qwen/oauth_creds.json) was discontinued on 2026-04-15. \
Use a DashScope API key (provider \"qwen\") or a Bailian Coding Plan key (provider \
\"qwen-coding\"). Stored tokens were not deleted. `prx auth logout --provider qwen-code` \
removes a PRX profile only.";

pub const COPILOT_LOGIN_DISABLED: &str = "\
GitHub Copilot login is disabled. PRX will not use VS Code's OAuth client, the private \
copilot_internal token endpoint, or Editor-Version: vscode headers. GitHub allows Copilot \
calls through an OAuth App you register for prx plus the Copilot SDK, which this Rust build \
does not embed. Saved Copilot cache files were not deleted.";

pub const OPENAI_DEVICE_CODE_UNSUPPORTED: &str = "\
Sign in with ChatGPT for open-source local apps documents loopback PKCE only \
(https://developers.openai.com/siwc/token-sharing-open-source/sign-in), not a device-code grant. \
Run `prx auth login --provider openai-codex` where a browser can reach \
http://127.0.0.1:1455/auth/callback. If that callback is missed, paste it with \
`prx auth paste-redirect --provider openai-codex`.";

pub const GLM_CODING_PLAN_WARNING: &str = "\
GLM Coding Plan endpoints are limited to officially listed tools. prx is not on that list, \
so use from cron, webhook, or daemon automation can be restricted or frozen. Prefer the \
standard pay-as-you-go GLM endpoint.";

pub const BAILIAN_CODING_PLAN_WARNING: &str = "\
Alibaba Bailian Coding Plan keys are for interactive coding tools and OpenClaw-style agents. \
Automation scripts, cron, webhooks, and daemon backends are outside that terms of use.";

pub const MINIMAX_OAUTH_WARNING: &str = "\
MiniMax OAuth is experimental. The default client id is the one OpenClaw registered \
(78257093-7e40-4613-99e0-527b14b39113). Set MINIMAX_OAUTH_CLIENT_ID to a client MiniMax issued \
for prx. Refresh uses https://account.minimax.io/oauth2/token (CN: https://account.minimaxi.com/oauth2/token), \
the account/oauth2 endpoints from OpenClaw PR 82056.";
