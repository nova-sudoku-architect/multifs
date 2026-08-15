use anyhow::Result;
use clap::Parser;

mod cli;
mod config;
mod error;
mod server;
mod storage;

#[derive(Parser)]
#[command(
    name = "multifs",
    version,
    about = "Multi-cloud storage pool with S3 API",
    long_about = "multifs is a multi-cloud storage pool that aggregates multiple cloud storage \
                   backends (pCloud, Box.net, S3-compatible, etc). Objects are distributed \
                   across backends and accounts. The service exposes an S3-compatible \
                   interface."
)]
enum Cli {
    /// Start the pCloudFS daemon
    Serve(cli::serve::ServeArgs),
    /// Initialize config and database
    Init(cli::init::InitArgs),
        /// Validate configuration and check all accounts
    Check(cli::status::CheckArgs),
    /// Manage configuration
    Config(cli::config_cmd::ConfigArgs),
    /// Manage pCloud accounts
    Account(cli::account_cmd::AccountArgs),
    /// Manage buckets
    Bucket(cli::bucket_cmd::BucketArgs),
    /// Manage objects
    Object(cli::object_cmd::ObjectArgs),
    /// Manage shard distribution
    Shard(cli::shard_cmd::ShardArgs),
    /// Show daemon health and account stats
    Status,
    /// Audit pCloud accounts — find files not managed by MultiFS
    Audit(cli::audit_cmd::AuditArgs),
    /// Import an existing pCloud file into MultiFS (register metadata only)
    Import(cli::import_cmd::ImportArgs),
    /// Manage content checksums (SHA-256) for managed blobs
    Checksum(cli::checksum_cmd::ChecksumArgs),
    /// Garbage-collect superseded and abandoned object versions
    Vacuum(cli::vacuum_cmd::VacuumArgs),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "multifs=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli {
        Cli::Serve(args) => cli::serve::run(args).await?,
        Cli::Init(args) => cli::init::run(args)?,
        Cli::Check(args) => cli::status::run_check(args.config).await?,
        Cli::Config(args) => cli::config_cmd::run(args)?,
        Cli::Account(args) => cli::account_cmd::run(args).await?,
        Cli::Bucket(args) => cli::bucket_cmd::run(args).await?,
        Cli::Object(args) => cli::object_cmd::run(args).await?,
        Cli::Shard(args) => cli::shard_cmd::run(args).await?,
        Cli::Status => cli::status::run_status().await?,
        Cli::Audit(args) => cli::audit_cmd::run(args).await?,
        Cli::Import(args) => cli::import_cmd::run(args).await?,
        Cli::Checksum(args) => cli::checksum_cmd::run(args).await?,
        Cli::Vacuum(args) => cli::vacuum_cmd::run(args).await?,
    }

    Ok(())
}
