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
    let mut backend = backend::RealBackend::new(cli.machine.clone(), ctx.socket.clone());
    match commands::execute(cli, &ctx, &mut backend) {
        // `run` stands in for the command it launched, so its exit code is
        // ours. The report rides along anyway: --json must not go silent just
        // because the verb also carries an exit code.
        Ok(commands::Outcome::Exit(code, report)) => {
            emit(report, json, quiet);
            // process::exit runs no destructors and flushes nothing; stdout is
            // a LineWriter, so anything not ending in a newline would be lost.
            let _ = std::io::Write::flush(&mut std::io::stdout());
            std::process::exit(code)
        }
        Ok(commands::Outcome::Report(report)) => {
            emit(report, json, quiet);
            std::process::ExitCode::SUCCESS
        }
        // --quiet suppresses output on success, not the reason for a failure:
        // swallowing this leaves a bare exit code and nothing to debug.
        Err(e) => {
            eprintln!("tty7: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn emit(report: commands::Report, json: bool, quiet: bool) {
    if quiet {
        return;
    }
    if json {
        if !report.json.is_null() {
            println!("{}", report.json);
        }
    } else if !report.human.is_empty() {
        print!("{}", ensure_newline(report.human));
    }
}

fn ensure_newline(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}
