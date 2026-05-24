//! CLI parsing for `NetMode` selection (Phase 2a of `plans/multiplayer.md`).
//!
//! Hand-rolled (no `clap`) — CivGame keeps `Cargo.toml` minimal and the surface
//! area here is tiny. We only need to read role + endpoint flags before the
//! Bevy `App` is built so `NetPlugin::init_resource::<NetMode>()` picks up the
//! caller's choice instead of the `Local` default.
//!
//! Recognised forms:
//!
//! ```text
//! cargo run                                       # Local
//! cargo run -- --listen --bind 0.0.0.0:5000       # ListenServer
//! cargo run -- --server --bind 0.0.0.0:5000       # DedicatedServer
//! cargo run -- --connect host:5000 --player NAME  # Client
//! ```
//!
//! Unknown flags (e.g. `--sandbox`) pass through unrecognised and are read by
//! their own startup code in `main.rs` — this parser is intentionally narrow.

use std::net::SocketAddr;

use bevy::prelude::Resource;

use super::{DisconnectPolicy, NetMode};

/// Parsed network configuration. Address fields stay raw `String` here so we
/// don't impose a particular socket-resolution policy on Phase 2 transports;
/// the Lightyear plumbing layer is responsible for resolving them.
#[derive(Resource, Debug, Clone, Default)]
pub struct NetConfig {
    pub mode: NetMode,
    /// Bind address for `ListenServer` / `DedicatedServer`. Pre-validated as
    /// parseable `SocketAddr` at CLI time so the server systems can `.unwrap`
    /// confidently when they need it.
    pub bind_addr: Option<SocketAddr>,
    /// `host:port` for `Client` mode. Same parse-at-CLI-time discipline.
    pub connect_addr: Option<SocketAddr>,
    /// Display name the client announces in `ClientHello`. `None` falls back
    /// to a generated default at connect time.
    pub player_name: Option<String>,
    /// What the server does when a client controlling a faction
    /// disconnects. Defaults to `AiTakeover`.
    pub on_disconnect: DisconnectPolicy,
}

/// Errors a malformed argv can produce. Caller decides whether to abort
/// (`main.rs` prints and exits) or recover. Distinct variants so callers can
/// match if they need to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// More than one of `--listen` / `--server` / `--connect` was passed.
    ConflictingModes,
    /// A flag that takes a value (`--bind`, `--connect`, `--player`) was
    /// the final argument or was followed by another flag.
    MissingValue(&'static str),
    /// `--bind` / `--connect` value didn't parse as `SocketAddr`.
    BadSocketAddr {
        flag: &'static str,
        value: String,
    },
    /// `--listen` or `--server` selected but no `--bind` provided.
    MissingBind,
    /// `--connect` selected but no address provided (the flag was present
    /// without an `host:port` argument).
    MissingConnectAddr,
    /// `--on-disconnect` got a value that wasn't a recognised policy.
    UnknownDisconnectPolicy(String),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::ConflictingModes => {
                write!(f, "only one of --listen / --server / --connect may be set")
            }
            CliError::MissingValue(flag) => write!(f, "{} requires a value", flag),
            CliError::BadSocketAddr { flag, value } => {
                write!(f, "{} expected host:port, got `{}`", flag, value)
            }
            CliError::MissingBind => write!(f, "--listen / --server require --bind host:port"),
            CliError::MissingConnectAddr => write!(f, "--connect requires a host:port argument"),
            CliError::UnknownDisconnectPolicy(v) => {
                write!(
                    f,
                    "--on-disconnect expects ai-takeover|pause|drop-faction, got `{}`",
                    v
                )
            }
        }
    }
}

/// Parse the process arg vector (skipping argv[0]) into a `NetConfig`.
/// Unknown tokens are ignored so other startup features (`--sandbox`,
/// future flags) coexist.
pub fn parse_from_env() -> Result<NetConfig, CliError> {
    parse_args(std::env::args().skip(1))
}

/// Same as `parse_from_env` but takes an explicit iterator, for tests.
pub fn parse_args<I, S>(args: I) -> Result<NetConfig, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let argv: Vec<String> = args.into_iter().map(Into::into).collect();
    let mut cfg = NetConfig::default();
    let mut listen = false;
    let mut server = false;
    let mut connect_raw: Option<String> = None;

    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        match arg {
            "--listen" => listen = true,
            "--server" => server = true,
            "--connect" => {
                let value = next_value(&argv, &mut i, "--connect")?;
                connect_raw = Some(value);
                continue;
            }
            "--bind" => {
                let value = next_value(&argv, &mut i, "--bind")?;
                let parsed = value
                    .parse::<SocketAddr>()
                    .map_err(|_| CliError::BadSocketAddr {
                        flag: "--bind",
                        value: value.clone(),
                    })?;
                cfg.bind_addr = Some(parsed);
                continue;
            }
            "--player" => {
                let value = next_value(&argv, &mut i, "--player")?;
                cfg.player_name = Some(value);
                continue;
            }
            "--on-disconnect" => {
                let value = next_value(&argv, &mut i, "--on-disconnect")?;
                let policy = DisconnectPolicy::parse(&value)
                    .ok_or_else(|| CliError::UnknownDisconnectPolicy(value.clone()))?;
                cfg.on_disconnect = policy;
                continue;
            }
            _ => {} // unknown — passes through (e.g. --sandbox)
        }
        i += 1;
    }

    let role_count = [listen, server, connect_raw.is_some()]
        .iter()
        .filter(|v| **v)
        .count();
    if role_count > 1 {
        return Err(CliError::ConflictingModes);
    }

    if let Some(raw) = connect_raw {
        let parsed = raw.parse::<SocketAddr>().map_err(|_| CliError::BadSocketAddr {
            flag: "--connect",
            value: raw.clone(),
        })?;
        cfg.connect_addr = Some(parsed);
        cfg.mode = NetMode::Client;
    } else if server {
        if cfg.bind_addr.is_none() {
            return Err(CliError::MissingBind);
        }
        cfg.mode = NetMode::DedicatedServer;
    } else if listen {
        if cfg.bind_addr.is_none() {
            return Err(CliError::MissingBind);
        }
        cfg.mode = NetMode::ListenServer;
    } else {
        cfg.mode = NetMode::Local;
    }

    Ok(cfg)
}

fn next_value(argv: &[String], cursor: &mut usize, flag: &'static str) -> Result<String, CliError> {
    let value_idx = *cursor + 1;
    let value = argv.get(value_idx).ok_or(CliError::MissingValue(flag))?;
    if value.starts_with("--") {
        return Err(CliError::MissingValue(flag));
    }
    *cursor = value_idx + 1;
    Ok(value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<NetConfig, CliError> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_args_defaults_local() {
        let cfg = parse(&[]).unwrap();
        assert_eq!(cfg.mode, NetMode::Local);
        assert!(cfg.bind_addr.is_none());
        assert!(cfg.connect_addr.is_none());
    }

    #[test]
    fn unknown_flag_is_ignored() {
        // --sandbox is parsed by main.rs, not us.
        let cfg = parse(&["--sandbox"]).unwrap();
        assert_eq!(cfg.mode, NetMode::Local);
    }

    #[test]
    fn listen_with_bind() {
        let cfg = parse(&["--listen", "--bind", "0.0.0.0:5000"]).unwrap();
        assert_eq!(cfg.mode, NetMode::ListenServer);
        assert_eq!(cfg.bind_addr.unwrap().port(), 5000);
    }

    #[test]
    fn server_with_bind() {
        let cfg = parse(&["--server", "--bind", "127.0.0.1:5050"]).unwrap();
        assert_eq!(cfg.mode, NetMode::DedicatedServer);
        assert_eq!(cfg.bind_addr.unwrap().port(), 5050);
    }

    #[test]
    fn listen_without_bind_errors() {
        let err = parse(&["--listen"]).unwrap_err();
        assert_eq!(err, CliError::MissingBind);
    }

    #[test]
    fn server_without_bind_errors() {
        let err = parse(&["--server"]).unwrap_err();
        assert_eq!(err, CliError::MissingBind);
    }

    #[test]
    fn connect_picks_client_mode() {
        let cfg = parse(&["--connect", "127.0.0.1:5000", "--player", "Alice"]).unwrap();
        assert_eq!(cfg.mode, NetMode::Client);
        assert_eq!(cfg.connect_addr.unwrap().port(), 5000);
        assert_eq!(cfg.player_name.as_deref(), Some("Alice"));
    }

    #[test]
    fn connect_missing_value_errors() {
        let err = parse(&["--connect"]).unwrap_err();
        assert_eq!(err, CliError::MissingValue("--connect"));
    }

    #[test]
    fn bind_bad_value_errors() {
        let err = parse(&["--listen", "--bind", "not-a-socket"]).unwrap_err();
        assert!(matches!(err, CliError::BadSocketAddr { flag: "--bind", .. }));
    }

    #[test]
    fn connect_bad_value_errors() {
        let err = parse(&["--connect", "not-a-socket"]).unwrap_err();
        assert!(matches!(err, CliError::BadSocketAddr { flag: "--connect", .. }));
    }

    #[test]
    fn listen_and_server_conflict() {
        let err = parse(&["--listen", "--server", "--bind", "0.0.0.0:5000"]).unwrap_err();
        assert_eq!(err, CliError::ConflictingModes);
    }

    #[test]
    fn listen_and_connect_conflict() {
        let err = parse(&[
            "--listen",
            "--bind",
            "0.0.0.0:5000",
            "--connect",
            "127.0.0.1:5000",
        ])
        .unwrap_err();
        assert_eq!(err, CliError::ConflictingModes);
    }

    #[test]
    fn flag_followed_by_flag_is_missing_value() {
        // `--bind` immediately followed by `--player` should reject — we
        // don't want `--bind --player Alice` to mis-parse as bind=Alice.
        let err = parse(&["--listen", "--bind", "--player", "Alice"]).unwrap_err();
        assert_eq!(err, CliError::MissingValue("--bind"));
    }

    #[test]
    fn on_disconnect_parses_each_policy() {
        for (flag, expected) in [
            ("ai-takeover", DisconnectPolicy::AiTakeover),
            ("pause", DisconnectPolicy::Pause),
            ("drop-faction", DisconnectPolicy::DropFaction),
        ] {
            let cfg = parse(&["--on-disconnect", flag]).unwrap();
            assert_eq!(cfg.on_disconnect, expected);
        }
    }

    #[test]
    fn on_disconnect_unknown_value_errors() {
        let err = parse(&["--on-disconnect", "explode"]).unwrap_err();
        assert!(matches!(err, CliError::UnknownDisconnectPolicy(_)));
    }

    #[test]
    fn on_disconnect_defaults_to_ai_takeover() {
        let cfg = parse(&[]).unwrap();
        assert_eq!(cfg.on_disconnect, DisconnectPolicy::AiTakeover);
    }

    #[test]
    fn player_persists_even_in_local_mode() {
        // Useful for `--sandbox --player Alice` once we add per-player
        // sandbox seeds; the field survives mode resolution.
        let cfg = parse(&["--player", "Alice"]).unwrap();
        assert_eq!(cfg.mode, NetMode::Local);
        assert_eq!(cfg.player_name.as_deref(), Some("Alice"));
    }
}
