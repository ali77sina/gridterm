use std::path::PathBuf;

/// Azure OpenAI configuration for the built-in agent. Loaded from environment
/// variables or a local `.env` file (both gitignored). No secrets are ever
/// hardcoded, so the repo is safe to make public.
#[derive(Clone)]
pub struct AiConfig {
    pub endpoint: String,
    pub deployment: String,
    pub api_version: String,
    pub api_key: String,
}

impl AiConfig {
    /// Load from process env first, then a local `.env` file.
    pub fn load() -> Option<Self> {
        // Pull any .env values into a map (does NOT override real env vars).
        let dotenv = load_dotenv();
        let get = |key: &str| -> Option<String> {
            std::env::var(key)
                .ok()
                .or_else(|| dotenv.get(key).cloned())
        };

        Some(Self {
            endpoint: get("AZURE_OPENAI_ENDPOINT")?,
            deployment: get("AZURE_OPENAI_DEPLOYMENT")?,
            api_version: get("AZURE_OPENAI_API_VERSION")
                .unwrap_or_else(|| "2024-10-21".into()),
            api_key: get("AZURE_OPENAI_KEY")?,
        })
    }

    /// Full chat-completions URL for streaming.
    pub fn chat_url(&self) -> String {
        format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.endpoint.trim_end_matches('/'),
            self.deployment,
            self.api_version
        )
    }
}

/// Parse a simple KEY=VALUE `.env` file (supports quotes, `#` comments, and an
/// optional leading `export`). Searches cwd and next to the binary.
fn load_dotenv() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let text = candidate_paths()
        .into_iter()
        .find_map(|p| std::fs::read_to_string(p).ok());
    let Some(text) = text else {
        return map;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            map.insert(k.trim().to_string(), v.to_string());
        }
    }
    map
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        paths.push(cwd.join(".env"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join(".env"));
            // target/release/../../.env (project root during dev)
            paths.push(dir.join("../../.env"));
        }
    }
    paths
}
