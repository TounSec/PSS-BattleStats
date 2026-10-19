use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pss_store::BattleStore;
use pss_store::paths::app_paths;

#[derive(Parser)]
#[command(
    name = "pss-capture",
    about = "MITM interception of Pixel Starships battle traffic"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a new self-signed root CA (cert + key), saved as PEM files. Run once, then trust
    /// the certificate file on the machine/client whose traffc will be intercepted.
    GenerateCa {
        #[arg(long, default_value_os_t = app_paths().default_ca_cert())]
        cert_out: PathBuf,
        #[arg(long, default_value_os_t = app_paths().default_ca_key())]
        key_out: PathBuf,
    },
    /// Run the MITM proxy, capturing FinaliseBattle requests into the battle store.
    Run {
        #[arg(long, default_value = "127.0.0.1:8484")]
        listen: SocketAddr,
        #[arg(long, default_value_os_t = app_paths().default_ca_cert())]
        ca_cert: PathBuf,
        #[arg(long, default_value_os_t = app_paths().default_ca_key())]
        ca_key: PathBuf,
        #[arg(long, default_value_t = app_paths().battles_db().to_string_lossy().into_owned())]
        db: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    // The default locations live in per-user folders that may not exist yet. Failing here isn't
    // fatal: explicit --db/--ca-cert/... paths elsewhere still work
    let _ = app_paths().ensure_dirs();

    match cli.command {
        Command::GenerateCa { cert_out, key_out } => {
            let ca = pss_capture::generate_ca()?;
            std::fs::write(&cert_out, &ca.cert_pem)
                .with_context(|| format!("Failed to write {}", cert_out.display()))?;
            std::fs::write(&key_out, &ca.key_pem)
                .with_context(|| format!("Failed to write {}", key_out.display()))?;
            println!(
                "Trust {} as a root CA on the machine whose traffic you'll intercept.",
                cert_out.display()
            );
        }
        Command::Run {
            listen,
            ca_cert,
            ca_key,
            db,
        } => {
            let ca_cert_pem = std::fs::read_to_string(&ca_cert)
                .with_context(|| format!("Failed to read {}", ca_cert.display()))?;
            let ca_key_pem = std::fs::read_to_string(&ca_key)
                .with_context(|| format!("Failed to read {}", ca_key.display()))?;
            let store = BattleStore::open(&db)?;

            println!("Listening on {listen} - point your PSS client's HTTP(S) proxy here.");
            pss_capture::run(listen, &ca_cert_pem, &ca_key_pem, store).await?;
        }
    }
    Ok(())
}
