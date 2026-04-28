use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::process::{Command, ExitCode};

use super_tabs_core::{PIPE_NAME, UpdatePayload};

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<OsString>) -> Result<(), String> {
    let command = parse_args(args)?;
    let payload = UpdatePayload {
        version: 1,
        pane_id: command.pane_id,
        updates: command.updates,
    };
    let payload_json = payload.to_json()?;

    let status = Command::new("zellij")
        .args(["action", "pipe", "--name", PIPE_NAME, "--", &payload_json])
        .status()
        .map_err(|error| format!("failed to launch `zellij`: {error}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("`zellij action pipe` exited with status {status}"))
    }
}

struct CliCommand {
    pane_id: u32,
    updates: BTreeMap<String, String>,
}

fn parse_args(args: Vec<OsString>) -> Result<CliCommand, String> {
    let mut iter = args.into_iter();
    let Some(subcommand) = iter.next() else {
        return Err(usage());
    };
    let subcommand = subcommand.to_string_lossy();

    if subcommand == "--help" || subcommand == "-h" {
        return Err(usage());
    }

    if subcommand != "set" {
        return Err(format!("unsupported command `{subcommand}`\n\n{}", usage()));
    }

    let mut pane_id = None;
    let mut updates = BTreeMap::new();

    while let Some(arg) = iter.next() {
        let arg = arg.to_string_lossy().to_string();
        if arg == "--help" || arg == "-h" {
            return Err(usage());
        }

        if arg == "--pane" {
            let value = iter
                .next()
                .ok_or_else(|| "`--pane` requires a numeric pane id".to_string())?;
            pane_id = Some(parse_pane_id(&value.to_string_lossy())?);
            continue;
        }

        let (key, value) = arg
            .split_once('=')
            .ok_or_else(|| format!("expected COLUMN=value argument, got `{arg}`"))?;
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("invalid empty column name in `{arg}`"));
        }
        updates.insert(key.to_string(), value.to_string());
    }

    if updates.is_empty() {
        return Err(format!("no column updates supplied\n\n{}", usage()));
    }

    let pane_id = match pane_id {
        Some(pane_id) => pane_id,
        None => {
            let pane_env = env::var("ZELLIJ_PANE_ID").map_err(|_| {
                "missing `ZELLIJ_PANE_ID`; pass `--pane <id>` when outside the target pane"
                    .to_string()
            })?;
            parse_pane_id(&pane_env)?
        }
    };

    Ok(CliCommand { pane_id, updates })
}

fn parse_pane_id(input: &str) -> Result<u32, String> {
    input
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("invalid pane id `{input}`"))
}

fn usage() -> String {
    "usage: super-tabs set [--pane <id>] column=value [column=value ...]".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_explicit_pane_updates() {
        let command = parse_args(vec![
            OsString::from("set"),
            OsString::from("--pane"),
            OsString::from("12"),
            OsString::from("branch=main"),
            OsString::from("status=dirty"),
        ])
        .unwrap();

        assert_eq!(command.pane_id, 12);
        assert_eq!(command.updates.get("branch").unwrap(), "main");
        assert_eq!(command.updates.get("status").unwrap(), "dirty");
    }
}
