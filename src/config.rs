use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyConfig {
    pub server: ServerConfig,
    pub upstream: UpstreamConfig,
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub credentials: CredentialsConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpstreamConfig {
    pub openai_api_url: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SchedulerConfig {
    #[serde(default = "default_global_concurrency")]
    pub global_concurrency: usize,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    pub classes: HashMap<String, TrafficClassConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TrafficClassConfig {
    pub weight: u32,
    pub min_concurrency: usize,
    pub max_concurrency: usize,
    pub max_queue_size: usize,
    /// Priority for eviction (higher = less likely to be evicted)
    /// If not specified, derived from weight
    #[serde(default)]
    pub priority: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CredentialsConfig {
    /// Map API keys to traffic class names
    #[serde(default)]
    pub api_keys: HashMap<String, String>,
    /// Default class for unknown credentials (if None, reject with 403)
    pub default_class: Option<String>,
    /// Default class for requests with invalid/misconfigured class names
    /// If None, treats invalid class as unknown credential (403)
    pub fallback_class: Option<String>,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_timeout_seconds() -> u64 {
    300
}

fn default_global_concurrency() -> usize {
    10
}

impl ProxyConfig {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let settings = config::Config::builder()
            .add_source(config::File::from(path.as_ref()))
            .build()?;

        let config = settings.try_deserialize()?;
        Ok(config)
    }

    pub fn from_env() -> Result<Self> {
        let settings = config::Config::builder()
            .add_source(config::File::with_name("config").required(false))
            .add_source(config::Environment::with_prefix("PROXY"))
            .build()?;

        let config = settings.try_deserialize()?;
        Ok(config)
    }
}
