use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum, parser::ValueSource};
use std::{num::NonZeroUsize, path::PathBuf};

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    #[arg(short, long, global = true, env = "LAYERLOCK_CONFIG", default_value = ".layerlock.toml", value_parser = nonempty_path)]
    pub config: PathBuf,
    #[arg(short, long, global = true, env = "LAYERLOCK_GROUPS", value_delimiter = ',', value_parser = nonempty_name)]
    pub group: Vec<String>,
    #[arg(
        long,
        global = true,
        env = "LAYERLOCK_REGISTRY_TIMEOUT",
        default_value = "30"
    )]
    pub registry_timeout: NonZeroUsize,
    #[arg(
        long,
        global = true,
        env = "LAYERLOCK_REGISTRY_CONCURRENCY",
        default_value = "8"
    )]
    pub registry_concurrency: NonZeroUsize,
    #[arg(
        long,
        global = true,
        env = "LAYERLOCK_OUTPUT",
        default_value = "human",
        value_enum
    )]
    pub output: Output,
    #[arg(
        long,
        global = true,
        env = "LAYERLOCK_COLOR",
        default_value = "auto",
        value_enum
    )]
    pub color: Color,
    #[arg(short, long, global = true)]
    pub verbose: bool,
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn parse_runtime() -> Self {
        Self::try_parse_runtime_from(std::env::args_os()).unwrap_or_else(|error| error.exit())
    }

    pub fn try_parse_runtime_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString>,
    {
        // Probe CLI sources without env, then disable overridden env bindings.
        // Clap otherwise validates inherited env values in subcommands even when
        // an overriding global CLI value was supplied before the subcommand.
        // Do not modify process environment (unsafe in threaded code).
        let args: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
        let ids = [
            "config",
            "group",
            "registry_timeout",
            "registry_concurrency",
            "output",
            "color",
        ];
        let mut probe = Self::command();
        for id in ids {
            probe = probe.mut_arg(id, |arg| arg.env(None::<&str>));
        }
        let mut command = Self::command();
        if let Ok(matches) = group_scopes(probe).try_get_matches_from(&args) {
            for id in ids {
                let supplied = matches.value_source(id) == Some(ValueSource::CommandLine)
                    || (id == "group"
                        && matches.subcommand().is_some_and(|(_, sub)| {
                            sub.value_source(id) == Some(ValueSource::CommandLine)
                        }));
                if supplied {
                    command = command.mut_arg(id, |arg| arg.env(None::<&str>));
                }
            }
        }
        let matches = group_scopes(command).try_get_matches_from(args)?;
        let mut cli = Self::from_arg_matches(&matches)?;
        if let Some((_, sub)) = matches.subcommand()
            && sub.value_source("group") == Some(ValueSource::CommandLine)
        {
            cli.group.extend(
                sub.get_many::<String>("group")
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        Ok(cli)
    }
}

fn group_scopes(mut command: clap::Command) -> clap::Command {
    // Clap's propagation replaces (rather than appends) a global Vec when it is
    // supplied on both sides of the subcommand. Parse the two scopes independently
    // and merge only CLI values above. Keep derive-generated parsers/env help.
    let group = command
        .get_arguments()
        .find(|arg| arg.get_id() == "group")
        .unwrap()
        .clone()
        .global(false);
    command = command.mut_arg("group", |arg| arg.global(false));
    for subcommand in command.get_subcommands_mut() {
        *subcommand = subcommand.clone().arg(group.clone());
    }
    command
}

fn nonempty_path(value: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        Err("path must not be empty".into())
    } else {
        Ok(value.into())
    }
}
fn nonempty_name(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        Err("group name must not be empty".into())
    } else {
        Ok(value.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Output {
    Human,
    Json,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Color {
    Auto,
    Always,
    Never,
}
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Ensure selected base images are published; do not edit Dockerfiles.
    Build {
        #[arg(long)]
        force: bool,
    },
    /// Preview image publication and Dockerfile updates without mutation.
    Check {
        #[arg(long)]
        force: bool,
    },
    /// Publish base images, then update marked application Dockerfiles.
    Sync {
        #[arg(long)]
        force: bool,
    },
}
impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Build { .. } => "build",
            Self::Check { .. } => "check",
            Self::Sync { .. } => "sync",
        }
    }
    pub fn force(&self) -> bool {
        match self {
            Self::Build { force } | Self::Check { force } | Self::Sync { force } => *force,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_options_and_groups() {
        let cli = Cli::try_parse_from([
            "layerlock",
            "check",
            "-g",
            "a,b",
            "-g",
            "a",
            "--output",
            "json",
            "--force",
        ])
        .unwrap();
        assert_eq!(cli.group, ["a", "b", "a"]);
        assert_eq!(cli.output, Output::Json);
        assert!(cli.command.force());
        let cli =
            Cli::try_parse_runtime_from(["layerlock", "-g", "a", "check", "-g", "b"]).unwrap();
        assert_eq!(cli.group, ["a", "b"]);
    }
    #[test]
    fn rejects_invalid_values() {
        for args in [
            vec!["--registry-timeout", "0"],
            vec!["--registry-concurrency", "-1"],
            vec!["--config", ""],
            vec!["--group", "a,"],
            vec!["--output", "yaml"],
        ] {
            let mut input = vec!["layerlock", "check"];
            input.extend(args);
            assert!(Cli::try_parse_from(input).is_err());
        }
    }
}
