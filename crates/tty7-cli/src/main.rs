mod address;
mod backend;
mod cli;
mod commands;
mod output;
mod resolve;
mod server;
#[cfg(test)]
mod testbed;

use clap::Parser;

fn main() -> std::process::ExitCode {
    let cli = cli::Cli::parse();
    let json = cli.json;
    let quiet = cli.quiet;
    let ctx = address::Context::from_env();
    let mut backend = backend::RealBackend::new(cli.machine.clone());
    match commands::execute(cli, &ctx, &mut backend) {
        Ok(commands::Outcome::Exit(code)) => std::process::exit(code),
        Ok(commands::Outcome::Report(report)) => {
            if json {
                if !report.json.is_null() {
                    println!("{}", report.json);
                }
            } else if !quiet && !report.human.is_empty() {
                print!("{}", ensure_newline(report.human));
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            if !quiet {
                eprintln!("tty7: {e:#}");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

fn ensure_newline(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}
