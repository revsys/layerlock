use layerlock::{
    cli::{Cli, Color, Command, Output},
    config,
    credentials::Credentials,
    docker::Docker,
    plan,
    registry::Registry,
};
use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
    time::Duration,
};

fn write_error(out: &mut impl Write, message: &str, color: bool) -> io::Result<()> {
    let heading = "error:";
    let width = message
        .lines()
        .map(|line| line.chars().count())
        .chain([heading.len()])
        .max()
        .unwrap();
    let border = format!("+{}+", "-".repeat(width + 2));
    let (red, reset) = if color {
        ("\x1b[1;31m", "\x1b[0m")
    } else {
        ("", "")
    };
    writeln!(out, "{red}{border}{reset}")?;
    for line in std::iter::once(heading).chain(message.lines()) {
        let padding = " ".repeat(width - line.chars().count());
        writeln!(out, "{red}|{reset} {line}{padding} {red}|{reset}")?;
    }
    writeln!(out, "{red}{border}{reset}")
}

fn main() -> ExitCode {
    let cli = Cli::parse_runtime();
    let mut report = plan::Report::new(&cli.command);
    let result = (|| -> anyhow::Result<()> {
        let loaded = config::LoadedConfig::load(&cli.config)?;
        plan::prepare(&loaded, &cli.group, &cli.command, &mut report)?;
        if cli.verbose {
            eprintln!(
                "Validated inputs for {} group(s); checking registry availability.",
                report.groups.len()
            );
        }
        let registry = Registry::new(
            Duration::from_secs(cli.registry_timeout.get() as u64),
            Credentials::load()?,
        )?;
        plan::resolve(
            &loaded,
            &cli.command,
            &registry,
            &Docker,
            cli.registry_concurrency.get(),
            &mut report,
        )?;
        if !matches!(cli.command, Command::Check { .. }) {
            if cli.verbose {
                eprintln!("Image plan complete; executing publication actions sequentially.");
            }
            plan::execute(&loaded, &cli.command, &registry, &Docker, &mut report)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        report.complete = false;
        report.errors.push(format!("{error:#}"));
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let output_result = match cli.output {
        Output::Json => serde_json::to_writer_pretty(&mut out, &report)
            .map_err(io::Error::other)
            .and_then(|()| writeln!(out)),
        Output::Human => (|| {
            let color = match cli.color {
                Color::Always => true,
                Color::Never => false,
                Color::Auto => {
                    stdout.is_terminal()
                        && std::env::var_os("NO_COLOR").is_none_or(|s| s.is_empty())
                }
            };
            for group in &report.groups {
                if color {
                    writeln!(
                        out,
                        "{}: \x1b[36m{}\x1b[0m — {}",
                        group.name,
                        group.expected_reference,
                        group.image_action.description(group.completed)
                    )?;
                } else {
                    writeln!(
                        out,
                        "{}: {} — {}",
                        group.name,
                        group.expected_reference,
                        group.image_action.description(group.completed)
                    )?;
                }
            }
            for change in &report.dockerfile_changes {
                if matches!(cli.command, Command::Check { .. }) {
                    write!(out, "{}", change.diff)?;
                } else {
                    writeln!(
                        out,
                        "{}: {}",
                        change.path,
                        if change.completed {
                            "updated"
                        } else {
                            "update not completed"
                        }
                    )?;
                }
            }
            let stderr = io::stderr();
            let error_color = match cli.color {
                Color::Always => true,
                Color::Never => false,
                Color::Auto => {
                    stderr.is_terminal()
                        && std::env::var_os("NO_COLOR").is_none_or(|s| s.is_empty())
                }
            };
            let mut err = stderr.lock();
            for error in &report.errors {
                write_error(&mut err, error, error_color)?;
            }
            Ok::<_, io::Error>(())
        })(),
    };
    if let Err(error) = output_result {
        eprintln!("error writing report: {error}");
        return ExitCode::from(2);
    }
    if !report.complete || !report.errors.is_empty() {
        ExitCode::from(2)
    } else if matches!(cli.command, Command::Check { .. }) && report.work_needed == Some(true) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
