use serde::{Deserialize, Serialize};

/// Placement strategy for distributing uploads across backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlacementStrategy {
    /// Round-robin across all backends (original behavior)
    RoundRobin,
    /// Pick the backend with the lowest utilization rate
    Utilization,
}

impl std::str::FromStr for PlacementStrategy {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "round-robin" | "round_robin" | "roundrobin" => Ok(Self::RoundRobin),
            "utilization" | "fill-level" | "fill_level" | "least-full" | "least_full" | "leastfull" => Ok(Self::Utilization),
            other => anyhow::bail!("Unknown placement strategy '{}'. Expected: round-robin or utilization", other),
        }
    }
}

impl std::fmt::Display for PlacementStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RoundRobin => write!(f, "round-robin"),
            Self::Utilization => write!(f, "utilization"),
        }
    }
}

fn default_placement_strategy() -> PlacementStrategy {
    PlacementStrategy::Utilization
}

/// Top-level configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Bind address
    #[serde(default = "default_bind")]
    pub bind: String,
    /// S3 API port
    #[serde(default = "default_s3_port")]
    pub s3_port: u16,
    /// NFS port
    #[serde(default = "default_nfs_port")]
    pub nfs_port: u16,
    /// Whether to enable NFS server
    #[serde(default = "default_true")]
    pub enable_nfs: bool,
    /// Whether to enable S3 server
    #[serde(default = "default_true")]
    pub enable_s3: bool,
    /// Whether to enable the read-only web UI
    #[serde(default)]
    pub enable_web: bool,
    /// Web UI port (read-only browser)
    #[serde(default = "default_web_port")]
    pub web_port: u16,
    /// Whether to enable the metrics/health endpoint listener (`/metrics`,
    /// `/healthz`, `/readyz`). Off by default — opt in to expose them.
    #[serde(default)]
    pub enable_metrics: bool,
    /// Metrics/health port (Prometheus scrape target)
    #[serde(default = "default_metrics_port")]
    pub metrics_port: u16,
    /// S3 SigV4 access keys as `ACCESS_KEY_ID:SECRET,AK2:SK2`. When non-empty,
    /// every S3 request must be signed with one of these keys (SignatureDoesNotMatch
    /// / AccessDenied on failure). When empty, the endpoint is **unauthenticated**
    /// (local/dev only) and a startup warning is emitted. Secrets belong in env vars,
    /// not the config file — set this from an env (e.g. `MULTIFS_ACCESS_KEYS`).
    #[serde(default)]
    pub access_keys: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Whether to enable TLS
    #[serde(default)]
    pub enabled: bool,
    /// Path to TLS certificate file
    pub cert_path: Option<String>,
    /// Path to TLS key file
    pub key_path: Option<String>,
    /// Domain name for TLS
    pub domain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Path to SQLite metadata database
    #[serde(default = "default_meta_db")]
    pub meta_db_path: String,
    /// Placement strategy for distributing uploads across backends.
    /// Options: "Utilization" (default; least-full account) or "RoundRobin".
    #[serde(default = "default_placement_strategy")]
    pub placement_strategy: PlacementStrategy,
    /// List of pCloud accounts
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    /// Background multipart coalescing. When true, the daemon periodically
    /// coalesces completed multipart objects into single blobs (removes per-part
    /// read round-trips + part-row retention). Off by default; also runnable
    /// on demand via `multifs object coalesce`. See `coalesce_multipart`.
    #[serde(default)]
    pub auto_coalesce_multipart: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    /// Email of the pCloud account (or a label for non-cloud backends)
    pub email: String,
    /// Backend type: "pcloud" (default), "local" (local disk)
    pub backend_type: Option<String>,
    /// Environment variable name that holds the OAuth token
    pub token_env: Option<String>,
    /// Base path prefix (e.g., "/multifs/00")
    pub mount_prefix: String,
    /// Quota in GB (optional, for display/placement decisions)
    pub quota_gb: Option<u64>,
    /// Local disk root directory (required for backend_type = "local")
    pub path: Option<String>,
    /// Placement priority (lower = preferred). Backends are filled from the
    /// lowest priority first, falling back to higher priorities only when the
    /// preferred tiers are full. Defaults: 0 for cloud backends, 1 for "local".
    pub priority: Option<u32>,
    /// Explicit OAuth token (insecure, prefer token_env)
    #[serde(skip)]
    pub token_override: Option<String>,
}

impl AccountConfig {
    /// Resolve the OAuth token from env or override
    pub fn resolve_token(&self) -> anyhow::Result<String> {
        if let Some(ref tok) = self.token_override {
            return Ok(tok.clone());
        }
        if let Some(ref env_name) = self.token_env {
            std::env::var(env_name)
                .map_err(|_| anyhow::anyhow!("Environment variable {} is not set", env_name))
        } else {
            anyhow::bail!("No token configured for account {}. Set token_env.", self.email)
        }
    }
}

// Defaults
fn default_bind() -> String { "0.0.0.0".to_string() }
fn default_s3_port() -> u16 { 9000 }
fn default_web_port() -> u16 { 9001 }
fn default_metrics_port() -> u16 { 9100 }
fn default_nfs_port() -> u16 { 2049 }
fn default_true() -> bool { true }
fn default_meta_db() -> String {
    "/var/lib/multifs/meta.db".to_string()
}


impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            s3_port: default_s3_port(),
            nfs_port: default_nfs_port(),
            enable_nfs: true,
            enable_s3: true,
            enable_web: false,
            web_port: default_web_port(),
            enable_metrics: false,
            metrics_port: default_metrics_port(),
            access_keys: String::new(),
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            meta_db_path: default_meta_db(),
            placement_strategy: default_placement_strategy(),
            accounts: Vec::new(),
            auto_coalesce_multipart: false,
        }
    }
}

/// Save configuration to a TOML file
pub fn save(path: &str, config: &Config) -> anyhow::Result<()> {
    let contents = toml::ser::to_string_pretty(config)
        .map_err(|e| anyhow::anyhow!("Failed to serialize config: {}", e))?;
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &contents)?;
    Ok(())
}

/// Load configuration from a TOML file
pub fn load(path: &str) -> anyhow::Result<Config> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read config file '{}': {}", path, e))?;
    let config: Config = toml::de::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("Failed to parse config '{}': {}", path, e))?;
    Ok(config)
}

/// Find the config file in standard locations
pub fn find_config() -> anyhow::Result<String> {
    let paths = vec![
        "/etc/multifs/config.toml".to_string(),
        "~/.multifs.toml".to_string(),
        "./multifs.toml".to_string(),
        "./config.toml".to_string(),
    ];
    let expanded: Vec<String> = paths
        .into_iter()
        .map(|p| shellexpand::tilde(&p).to_string())
        .collect();

    for path in &expanded {
        if std::path::Path::new(path).exists() {
            return Ok(path.clone());
        }
    }
    anyhow::bail!(
        "No config file found. Run 'multifs init' to create one, or specify with --config."
    );
}
