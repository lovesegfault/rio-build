use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Unique identifier for a builder
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BuilderId(String);

impl BuilderId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BuilderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for BuilderId {
    fn default() -> Self {
        Self::new()
    }
}

/// Unique identifier for a build job
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(String);

impl JobId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn from_string(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

/// Platform/system type
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Platform {
    X86_64Linux,
    Aarch64Linux,
    X86_64Darwin,
    Aarch64Darwin,
    Other(String),
}

impl Platform {
    pub fn from_str(s: &str) -> Self {
        match s {
            "x86_64-linux" => Platform::X86_64Linux,
            "aarch64-linux" => Platform::Aarch64Linux,
            "x86_64-darwin" => Platform::X86_64Darwin,
            "aarch64-darwin" => Platform::Aarch64Darwin,
            other => Platform::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Platform::X86_64Linux => "x86_64-linux",
            Platform::Aarch64Linux => "aarch64-linux",
            Platform::X86_64Darwin => "x86_64-darwin",
            Platform::Aarch64Darwin => "aarch64-darwin",
            Platform::Other(s) => s,
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
