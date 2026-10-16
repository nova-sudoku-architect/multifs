use clap::Parser;
use anyhow::Result;

/// Manage configuration
#[derive(Parser)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigSubcommand,
}

#[derive(Parser)]
pub enum ConfigSubcommand {
    /// Show current configuration
    Show,
    /// Get one scalar config value (dot-notation key)
    Get {
        /// Config key (e.g. "server.s3_port")
        key: String,
    },
    /// Set a scalar config value (dot-notation key)
    ///
    /// Validates the value, writes the config atomically (temp file + rename),
    /// and keeps a `.bak` of the previous file. Secrets (like
    /// `server.access_keys`) should be supplied via the environment, not argv.
    Set {
        /// Config key (e.g. "server.s3_port")
        key: String,
        /// Config value
        value: String,
    },
}

pub fn run(args: ConfigArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let mut cfg = crate::config::load(&cfg_path)?;

    match args.command {
        ConfigSubcommand::Show => {
            let toml_str = toml::to_string_pretty(&cfg)?;
            println!("{}", toml_str);
        }
        ConfigSubcommand::Get { key } => match cfg.get_key(&key) {
            Some(v) => println!("{}", v),
            None => anyhow::bail!("Unknown config key: {}", key),
        },
        ConfigSubcommand::Set { key, value } => {
            cfg.set_key(&key, &value)?;
            crate::config::save_atomic(&cfg_path, &cfg)?;
            // Never echo secret material.
            let shown = if key == "server.access_keys" {
                "<redacted>".to_string()
            } else {
                value.clone()
            };
            println!("✅ Set {} = {}", key, shown);
            println!("   Config: {}", cfg_path);
            println!("   (previous config backed up to {}.bak)", cfg_path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::config::PlacementStrategy;

    #[test]
    fn test_set_scalar_keys() {
        let mut cfg = crate::config::Config::default();
        cfg.set_key("server.s3_port", "9123").unwrap();
        assert_eq!(cfg.server.s3_port, 9123);
        cfg.set_key("server.enable_web", "true").unwrap();
        assert!(cfg.server.enable_web);
        cfg.set_key("server.enable_web", "off").unwrap();
        assert!(!cfg.server.enable_web);
        cfg.set_key("storage.placement_strategy", "round-robin").unwrap();
        assert_eq!(cfg.storage.placement_strategy, PlacementStrategy::RoundRobin);
        cfg.set_key("storage.placement_strategy", "Utilization").unwrap();
        assert_eq!(cfg.storage.placement_strategy, PlacementStrategy::Utilization);
        cfg.set_key("tls.domain", "files.example.com").unwrap();
        assert_eq!(cfg.tls.domain.as_deref(), Some("files.example.com"));
        cfg.set_key("tls.domain", "").unwrap();
        assert_eq!(cfg.tls.domain, None);
    }

    #[test]
    fn test_set_rejects_bad_values() {
        let mut cfg = crate::config::Config::default();
        assert!(cfg.set_key("server.s3_port", "notaport").is_err());
        assert!(cfg.set_key("server.enable_web", "maybe").is_err());
        assert!(cfg.set_key("storage.placement_strategy", "bogus").is_err());
        assert!(cfg.set_key("storage.accounts", "x").is_err());
        assert!(cfg.set_key("nope.key", "x").is_err());
    }

    #[test]
    fn test_get_scalar_keys() {
        let mut cfg = crate::config::Config::default();
        assert_eq!(cfg.get_key("server.s3_port").as_deref(), Some("9000"));
        cfg.set_key("server.s3_port", "1234").unwrap();
        assert_eq!(cfg.get_key("server.s3_port").as_deref(), Some("1234"));
        assert_eq!(cfg.get_key("does.not.exist"), None);
    }

    #[test]
    fn test_get_access_keys_redacted() {
        let mut cfg = crate::config::Config::default();
        cfg.set_key("server.access_keys", "AKID:SECRET,AKID2:SECRET2").unwrap();
        let got = cfg.get_key("server.access_keys").unwrap();
        assert!(!got.contains("SECRET"), "secret leaked: {}", got);
        assert!(got.contains("2 key(s)"));
    }

    #[test]
    fn test_save_atomic_roundtrip_and_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let path_str = path.to_string_lossy().to_string();

        let mut cfg = crate::config::Config::default();
        cfg.set_key("server.s3_port", "9001").unwrap();
        crate::config::save_atomic(&path_str, &cfg).unwrap();
        assert_eq!(crate::config::load(&path_str).unwrap().server.s3_port, 9001);

        // Second save keeps a .bak of the first.
        cfg.set_key("server.s3_port", "9002").unwrap();
        crate::config::save_atomic(&path_str, &cfg).unwrap();
        assert!(std::path::Path::new(&format!("{}.bak", path_str)).exists());
        assert_eq!(crate::config::load(&path_str).unwrap().server.s3_port, 9002);
    }

    #[test]
    fn test_upsert_env_file_preserves_and_updates() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join("multifs.env");
        let env_str = env_path.to_string_lossy().to_string();
        std::fs::write(&env_str, "A=1\nPCLOUD_TOKEN_X=old\nB=2\n").unwrap();

        crate::config::upsert_env_file(&env_str, "PCLOUD_TOKEN_X", "new").unwrap();
        let body = std::fs::read_to_string(&env_str).unwrap();
        assert!(body.contains("A=1"));
        assert!(body.contains("B=2"));
        assert!(body.contains("PCLOUD_TOKEN_X=new"));
        assert!(!body.contains("old"));
        assert_eq!(body.matches("PCLOUD_TOKEN_X=").count(), 1, "must not duplicate: {}", body);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&env_str).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "env file must be 0600");
        }
    }

    #[test]
    fn test_upsert_env_file_creates_missing() {
        let dir = tempfile::tempdir().unwrap();
        let env_str = dir.path().join("sub").join("multifs.env");
        let env_str = env_str.to_string_lossy().to_string();
        crate::config::upsert_env_file(&env_str, "K", "v").unwrap();
        assert_eq!(std::fs::read_to_string(&env_str).unwrap(), "K=v\n");
    }
}
