//! Provider display names for agents that report only a provider id.

/// A provider's name as people say it. Agents report only the id
/// (`openai-codex`); a provider not listed here reads as its id in words
/// (`foo-bar` becomes `Foo Bar`).
pub(crate) fn provider_name(id: &str) -> String {
    let known = match id {
        "openai-codex" => "Codex",
        "opencode" => "OpenCode",
        "opencode-go" => "OpenCode Go",
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "azure-openai-responses" => "Azure OpenAI",
        "github-copilot" => "Copilot",
        "google" => "Google",
        "google-gemini-cli" => "Gemini CLI",
        "google-antigravity" => "Antigravity",
        "google-vertex" => "Vertex AI",
        "amazon-bedrock" => "Bedrock",
        "openrouter" => "OpenRouter",
        "vercel-ai-gateway" => "Vercel AI Gateway",
        "xai" => "xAI",
        "groq" => "Groq",
        "cerebras" => "Cerebras",
        "mistral" => "Mistral",
        "minimax" => "MiniMax",
        "minimax-cn" => "MiniMax China",
        "huggingface" => "Hugging Face",
        "zai" => "Z.ai",
        "kimi-coding" => "Kimi",
        _ => "",
    };
    if !known.is_empty() {
        return known.into();
    }
    id.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}
