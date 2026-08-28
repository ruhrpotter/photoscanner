mod cli;
mod gui;
mod gui_settings;

use clap::Parser;

fn main() -> std::process::ExitCode {
    if let Err(error) = photoscanner::i18n::initialize() {
        eprintln!("Could not initialize translations: {error}");
    }
    let cli = cli::Cli::parse();
    match cli.command {
        None | Some(cli::Command::Gui) => {
            gui::run();
            std::process::ExitCode::SUCCESS
        }
        Some(command) => match cli::run(command) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!(
                    "{}",
                    photoscanner::i18n::tr_args("Error: {error}", &[("error", error.to_string())],)
                );
                std::process::ExitCode::from(2)
            }
        },
    }
}
