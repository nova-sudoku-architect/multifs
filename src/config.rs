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

/// Save configuration **atomically**: serialize, validate that it round-trips,
/// keep a `.bak` of the previous file, then write to a temp file in the same
/// directory and `rename` it into place (rename is atomic on POSIX). A crash
/// mid-write therefore never leaves a truncated/half-written config.
pub fn save_atomic(path: &str, config: &Config) -> anyhow::Result<()> {
    let contents = toml::ser::to_string_pretty(config)
        .map_err(|e| anyhow::anyhow!("Failed to serialize config: {}", e))?;
    // Validate: the serialized config must parse back cleanly before we touch
    // the real file. Refuse to write something we can't reload.
    toml::de::from_str::<Config>(&contents)
        .map_err(|e| anyhow::anyhow!("refusing to write an invalid config: {}", e))?;

    let p = std::path::Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // Keep a backup of the previous config (best-effort).
    if p.exists() {
        let bak = format!("{}.bak", path);
        let _ = std::fs::copy(p, &bak);
    }
    let tmp = format!("{}.tmp.{}", path, std::process::id());
    std::fs::write(&tmp, &contents)?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

/// Upsert `KEY=value` in an env file (`/etc/multifs.env` style), preserving all
/// other lines, creating the file with mode **0600** (secrets must not be
/// world-readable). Returns the (possibly created) path.
pub fn upsert_env_file(path: &str, key: &str, value: &str) -> anyhow::Result<()> {
    let mut lines: Vec<String> = match std::fs::read_to_string(path) {
        Ok(s) => s.lines().map(|l| l.to_string()).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(anyhow::anyhow!("reading {}: {}", path, e)),
    };
    let prefix = format!("{}=", key);
    let new_line = format!("{}={}", key, value);
    let mut replaced = false;
    for line in lines.iter_mut() {
        if line.trim_start().starts_with(&prefix) {
            *line = new_line.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.push(new_line);
    }
    let mut body = lines.join("\n");
    body.push('\n');

    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

impl Config {
    /// Set a scalar config value by dot-notation key (e.g. `server.s3_port`).
    /// Only a curated allow-list of scalar keys is settable — list/account edits
    /// go through the `account`/`link`/`folder` commands.
    pub fn set_key(&mut self, key: &str, value: &str) -> anyhow::Result<()> {
        use std::str::FromStr;
        let parse_bool = |v: &str| match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            other => anyhow::bail!("expected a boolean, got '{}'", other),
        };
        let parse_u16 = |v: &str| -> anyhow::Result<u16> {
            v.parse::<u16>().map_err(|_| anyhow::anyhow!("expected a port number, got '{}'", v))
        };
        let opt_str = |v: &str| -> Option<String> {
            if v.is_empty() || v == "none" || v == "null" { None } else { Some(v.to_string()) }
        };

        match key {
            "server.bind" => self.server.bind = value.to_string(),
            "server.s3_port" => self.server.s3_port = parse_u16(value)?,
            "server.nfs_port" => self.server.nfs_port = parse_u16(value)?,
            "server.enable_s3" => self.server.enable_s3 = parse_bool(value)?,
            "server.enable_nfs" => self.server.enable_nfs = parse_bool(value)?,
            "server.enable_web" => self.server.enable_web = parse_bool(value)?,
            "server.web_port" => self.server.web_port = parse_u16(value)?,
            "server.enable_metrics" => self.server.enable_metrics = parse_bool(value)?,
            "server.metrics_port" => self.server.metrics_port = parse_u16(value)?,
            "server.access_keys" => self.server.access_keys = value.to_string(),
            "tls.enabled" => self.tls.enabled = parse_bool(value)?,
            "tls.cert_path" => self.tls.cert_path = opt_str(value),
            "tls.key_path" => self.tls.key_path = opt_str(value),
            "tls.domain" => self.tls.domain = opt_str(value),
            "storage.meta_db_path" => self.storage.meta_db_path = value.to_string(),
            "storage.placement_strategy" => {
                self.storage.placement_strategy = PlacementStrategy::from_str(value)?
            }
            "storage.auto_coalesce_multipart" => {
                self.storage.auto_coalesce_multipart = parse_bool(value)?
            }
            other => anyhow::bail!(
                "unknown or non-scalar config key '{}'. Settable scalar keys: \
                 server.bind, server.s3_port, server.nfs_port, server.enable_s3, \
                 server.enable_nfs, server.enable_web, server.web_port, \
                 server.enable_metrics, server.metrics_port, server.access_keys, \
                 tls.enabled, tls.cert_path, tls.key_path, tls.domain, \
                 storage.meta_db_path, storage.placement_strategy, \
                 storage.auto_coalesce_multipart",
                other
            ),
        }
        Ok(())
    }

    /// Read a scalar config value by dot-notation key (for `config get`).
    pub fn get_key(&self, key: &str) -> Option<String> {
        Some(match key {
            "server.bind" => self.server.bind.clone(),
            "server.s3_port" => self.server.s3_port.to_string(),
            "server.nfs_port" => self.server.nfs_port.to_string(),
            "server.enable_s3" => self.server.enable_s3.to_string(),
            "server.enable_nfs" => self.server.enable_nfs.to_string(),
            "server.enable_web" => self.server.enable_web.to_string(),
            "server.web_port" => self.server.web_port.to_string(),
            "server.enable_metrics" => self.server.enable_metrics.to_string(),
            "server.metrics_port" => self.server.metrics_port.to_string(),
            "server.access_keys" => {
                // Never print key material — report only how many are configured.
                let n = self.server.access_keys.split(',').filter(|s| !s.trim().is_empty()).count();
                return Some(format!("<{} key(s) configured (redacted)>", n));
            }
            "tls.enabled" => self.tls.enabled.to_string(),
            "tls.cert_path" => self.tls.cert_path.clone().unwrap_or_default(),
            "tls.key_path" => self.tls.key_path.clone().unwrap_or_default(),
            "tls.domain" => self.tls.domain.clone().unwrap_or_default(),
            "storage.meta_db_path" => self.storage.meta_db_path.clone(),
            "storage.placement_strategy" => self.storage.placement_strategy.to_string(),
            "storage.auto_coalesce_multipart" => {
                self.storage.auto_coalesce_multipart.to_string()
            }
            _ => return None,
        })
    }
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
        // Documented + packaged location (see README / packaging).
        "/etc/multifs.toml".to_string(),
        // Legacy/alternate location (also kept for backwards compatibility).
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
