mod paths;
mod speech;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Name under which speech-dispatcher starts the output module. The binary behaves as the
/// module when invoked through a symlink with this name.
const MODULE_BINARY_NAME: &str = "sd_rconnect";

#[derive(Parser)]
#[command(
    version,
    about = "Control this computer from a phone, through its screen reader"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run as a speech-dispatcher output module (normally started by speech-dispatcher
    /// through the `sd_rconnect` symlink).
    SpeechModule {
        /// Module configuration file, passed by speech-dispatcher.
        config: Option<PathBuf>,
    },
    /// Development tool: stand in for the daemon and the phone, print what the speech
    /// module sends and simulate playback.
    SpeechMonitor {
        /// Also keep speaking locally (mirror mode). Without this flag the computer's own
        /// speech is silenced while the monitor runs.
        #[arg(long)]
        mirror: bool,
        /// Simulated speaking speed, in milliseconds per character.
        #[arg(long, default_value_t = 60)]
        ms_per_char: u64,
    },
}

fn main() -> anyhow::Result<()> {
    let invoked_as = std::env::args_os()
        .next()
        .map(PathBuf::from)
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });
    if invoked_as.as_deref() == Some(MODULE_BINARY_NAME) {
        return speech::module::run(std::env::args_os().nth(1).map(PathBuf::from));
    }

    match Cli::parse().command {
        Command::SpeechModule { config } => speech::module::run(config),
        Command::SpeechMonitor {
            mirror,
            ms_per_char,
        } => speech::monitor::run(mirror, ms_per_char),
    }
}
