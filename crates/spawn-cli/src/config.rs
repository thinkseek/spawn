//! Environment-based runtime configuration.

/// Process configuration read from environment variables.
#[derive(Debug, Clone)]
pub struct EnvConfig {
    /// PostgreSQL connection string.
    pub database_url: String,
}

impl EnvConfig {
    /// Build config from the process environment.
    ///
    /// # Errors
    /// Fails when a required variable is missing or empty.
    pub fn from_env() -> Result<Self, String> {
        Ok(Self {
            database_url: std::env::var("SPAWN_DATABASE_URL")
                .map_err(|_| "SPAWN_DATABASE_URL is required".to_string())?,
        })
    }

    /// Read a required environment variable.
    ///
    /// # Errors
    /// Fails when the variable is missing or empty.
    pub fn require(&self, key: &str) -> Result<String, String> {
        let _ = self;
        std::env::var(key).map_err(|_| format!("{key} is required"))
    }
}
