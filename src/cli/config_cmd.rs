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
    /// Set a configuration value (dot-notation key)
    Set {
        /// Config key (e.g. "server.s3_port")
        key: String,
        /// Config value
        value: String,
    },
}

pub fn run(args: ConfigArgs) -> Result<()> {
    let cfg_path = crate::config::find_config()?;
    let cfg = crate::config::load(&cfg_path)?;

    match args.command {
        ConfigSubcommand::Show => {
            let toml_str = toml::to_string_pretty(&cfg)?;
            println!("{}", toml_str);
        }
        ConfigSubcommand::Set { key, value } => {
            tracing::warn!(
                "Config set not fully implemented yet. Edit {} directly.",
                cfg_path
            );
            println!("Set {} = {}", key, value);
        }
    }

    Ok(())
}
