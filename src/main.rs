mod cli;
mod gui;

use clap::Parser;

fn main() -> std::process::ExitCode {
    let cli = cli::Cli::parse();
    match cli.command {
        None | Some(cli::Command::Gui) => {
            gui::run();
            std::process::ExitCode::SUCCESS
        }
        Some(command) => match cli::run(command) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Fehler: {error}");
                std::process::ExitCode::from(2)
            }
        },
    }
}
