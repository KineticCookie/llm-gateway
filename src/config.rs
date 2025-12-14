use serde::{Deserialize, Serialize};
use std::path::Path;
use anyhow::Result;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyConfig {
    pub server: ServerConfig,
    pub upstream: UpstreamConfig,
    pub queue: QueueConfig,
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
pub struct QueueConfig {
    #[serde(default = "default_max_queue_size")]
    pub max_queue_size: usize,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_concurrent_limit")]
    pub concurrent_limit: usize,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_max_queue_size() -> usize {
    100
}

fn default_timeout_seconds() -> u64 {
    300
}

fn default_concurrent_limit() -> usize {
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
