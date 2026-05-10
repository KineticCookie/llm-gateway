use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyConfig {
    pub server: ServerConfig,
    pub upstream: UpstreamConfig,
    /// Number of llama.cpp inference slots
    pub slots: usize,
    /// Default request timeout if not specified per project
    #[serde(with = "humantime_serde")]
    pub default_timeout: Duration,
    /// What to do with requests that have no matching API key
    /// "reject" = 403, or a project name to assign them to
    #[serde(default = "default_unauthenticated")]
    pub unauthenticated: UnauthenticatedPolicy,
    pub projects: HashMap<String, ProjectConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum UnauthenticatedPolicy {
    Reject(RejectLiteral),
    Project(String),
}

/// Marker type so "reject" deserializes to the enum variant
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RejectLiteral {
    Reject,
}

impl UnauthenticatedPolicy {
    pub fn project_name(&self) -> Option<&str> {
        match self {
            UnauthenticatedPolicy::Project(name) => Some(name.as_str()),
            UnauthenticatedPolicy::Reject(_) => None,
        }
    }
}

fn default_unauthenticated() -> UnauthenticatedPolicy {
    UnauthenticatedPolicy::Reject(RejectLiteral::Reject)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProjectConfig {
    /// Strict tier ordering — lower number = processed first.
    /// Projects in tier 1 are fully served before tier 2 gets any slots.
    pub priority: u32,
    /// Relative weight within the same priority tier.
    /// Normalized across peers — e.g. share:2 + share:1 = 67% / 33%.
    /// Defaults to 1.
    #[serde(default = "default_share")]
    pub share: u32,
    /// Hard cap on concurrent slots this project can use.
    /// Defaults to global slots count.
    pub max_slots: Option<usize>,
    /// Per-project request timeout (queue time + processing).
    /// Falls back to global default_timeout if not set.
    #[serde(default, with = "humantime_serde::option")]
    pub timeout: Option<Duration>,
    /// API keys that map to this project.
    #[serde(default)]
    pub api_keys: Vec<String>,
}

fn default_share() -> u32 {
    1
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
    pub url: String,
    pub api_key: Option<String>,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8080
}

impl ProxyConfig {
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let settings = config::Config::builder()
            .add_source(config::File::from(path.as_ref()))
            .build()?;

        let cfg: Self = settings.try_deserialize()?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_env() -> Result<Self> {
        let settings = config::Config::builder()
            .add_source(config::File::with_name("config").required(false))
            .add_source(config::Environment::with_prefix("GATEWAY").separator("__"))
            .build()?;

        let cfg: Self = settings.try_deserialize()?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.slots > 0, "slots must be > 0");
        anyhow::ensure!(
            !self.projects.is_empty(),
            "at least one project must be configured"
        );

        // Validate unauthenticated policy refers to a real project
        if let Some(project_name) = self.unauthenticated.project_name() {
            anyhow::ensure!(
                self.projects.contains_key(project_name),
                "unauthenticated project '{}' does not exist",
                project_name
            );
        }

        // Validate each project
        let mut seen_keys: HashSet<&str> = HashSet::new();
        for (name, project) in &self.projects {
            anyhow::ensure!(
                project.priority >= 1,
                "project '{}': priority must be >= 1",
                name
            );
            anyhow::ensure!(project.share >= 1, "project '{}': share must be >= 1", name);

            if let Some(max_slots) = project.max_slots {
                anyhow::ensure!(max_slots >= 1, "project '{}': max_slots must be >= 1", name);
                anyhow::ensure!(
                    max_slots <= self.slots,
                    "project '{}': max_slots ({}) exceeds global slots ({})",
                    name,
                    max_slots,
                    self.slots
                );
            }

            for key in &project.api_keys {
                anyhow::ensure!(
                    seen_keys.insert(key.as_str()),
                    "api key '{}' is assigned to multiple projects",
                    key
                );
            }
        }

        Ok(())
    }

    /// Resolve the effective max_slots for a project
    pub fn effective_max_slots(&self, project: &ProjectConfig) -> usize {
        project.max_slots.unwrap_or(self.slots)
    }

    /// Resolve the effective timeout for a project
    pub fn effective_timeout(&self, project: &ProjectConfig) -> Duration {
        project.timeout.unwrap_or(self.default_timeout)
    }

    /// Look up which project an API key belongs to
    pub fn project_for_key(&self, api_key: &str) -> Option<&str> {
        for (name, project) in &self.projects {
            if project.api_keys.iter().any(|k| k == api_key) {
                return Some(name.as_str());
            }
        }
        None
    }
}
