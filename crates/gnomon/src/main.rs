//! The `gnomon` command line.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gnomon::migrations::{self, EmbeddingDimension, Target};

#[derive(Parser)]
#[command(name = "gnomon", version, about = "gnomon's command line")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Work with the Store's migrations.
    Migrations {
        #[command(subcommand)]
        command: MigrationsCommand,
    },
}

#[derive(Subcommand)]
enum MigrationsCommand {
    /// Write the migrations into a consumer's migration chain.
    Export {
        /// The chain to write for.
        #[arg(long)]
        target: Target,
        /// The width of every embedding vector column, 1 to 16000.
        #[arg(long)]
        embedding_dimension: EmbeddingDimension,
        /// The directory to write the migration files into.
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Migrations {
            command:
                MigrationsCommand::Export {
                    target,
                    embedding_dimension,
                    out,
                },
        } => match migrations::export(target, embedding_dimension, &out) {
            Ok(paths) => {
                for path in paths {
                    println!("{}", path.display());
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("gnomon: {e}");
                ExitCode::FAILURE
            }
        },
    }
}
