//! Runtime configuration, loaded from environment variables at startup.
//!
//! The model fields (`BASE_URL`, `API_KEY`, `MODEL_NAME`) describe an
//! OpenAI-compatible endpoint for the (forthcoming) real agent client.
//! `EXA_API_KEY` powers the web-search tool.

#[derive(Clone)]
pub struct Config {
    /// Base URL of the OpenAI-compatible model API.
    pub base_url: String,
    /// Bearer key for the model API.
    pub api_key: Option<String>,
    /// Model identifier to send on completions.
    pub model: String,
    /// API key for Exa web search.
    pub exa_api_key: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            base_url: env_or("BASE_URL", "https://api.openai.com/v1"),
            api_key: env_opt("API_KEY"),
            model: env_or("MODEL_NAME", "(MODEL_NAME unset)"),
            exa_api_key: env_opt("EXA_API_KEY"),
        }
    }

    /// Web search is only usable when an Exa key is present.
    pub fn web_search_enabled(&self) -> bool {
        self.exa_api_key.is_some()
    }

    /// Just the host part of `base_url`, for compact display.
    pub fn base_host(&self) -> &str {
        self.base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or(&self.base_url)
    }
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_or(key: &str, default: &str) -> String {
    env_opt(key).unwrap_or_else(|| default.to_string())
}
