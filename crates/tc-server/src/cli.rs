//! Operator commands, run against the database rather than the API.
//!
//! # Why these exist
//!
//! A workspace with registration closed and nobody in it is unreachable: only
//! an administrator may mint an invite, and there is no administrator. The
//! documented workaround — start open, register, restart closed — has a window
//! in which anyone who finds the address becomes the administrator, which is
//! precisely the thing closing registration was meant to prevent.
//!
//! # Why locally, and not over the network
//!
//! The alternatives were a setup token in the logs, in an environment variable,
//! or in a file, redeemed over HTTP. Each of those is a **live credential on the
//! network**: something to mint, store, expire, and refuse after use, all of
//! which is code that can be wrong, defending a door this approach never opens.
//!
//! Running here needs no authentication, and the reason is not convenience.
//! Anyone who can execute this binary against the database can already read
//! every message in it — the store is one file and there is no encryption at
//! rest. Filesystem access *is* the authentication, and these commands grant
//! nothing that it did not already grant. What they add is a supported way to
//! do it, in place of the `sqlite3 "UPDATE users SET admin = 1"` the README used
//! to recommend.
//!
//! # Why an invite rather than an account
//!
//! `invite` mints a link instead of creating an administrator outright, so the
//! first person picks their own handle and password and never types a password
//! into a shell. It also reuses the redemption path exactly, including the rule
//! that the first human to register becomes the administrator — so bootstrapping
//! needs no special case anywhere in the store or the API.

use std::path::Path;

use tc_core::{Id, IdGen, now_ms};
use tc_store::{NewInvite, Store};

use crate::auth;
use crate::config::Config;

pub const HELP: &str = "\
tensorchat — team chat that runs on one box

USAGE
  tc-server                     Run the server. This is the default.
  tc-server invite [options]    Mint an invite link and print it once.
  tc-server promote <handle>    Grant administrator.
  tc-server demote <handle>     Revoke administrator.
  tc-server help

INVITE OPTIONS
  --uses N     How many accounts the link may create. 0 is unlimited.
               Default 1.
  --days N     Days until it expires. 0 never expires. Default 7.
  --label TEXT What the link is for, so a stale one is identifiable later.
  --url ORIGIN Public origin for the printed link, e.g. https://chat.example.com
               Defaults to TC_PUBLIC_URL, then to the bind address.

Every command reads the same environment as the server (TC_DB and friends), so
it acts on the same database with no extra arguments.

  tc-server invite --uses 1 --days 2
  docker compose exec tensorchat tc-server invite
";

/// What the process was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Run the server. The default, and what a bare invocation means.
    Serve,
    Invite(InviteArgs),
    /// Grant or revoke administrator.
    SetAdmin {
        handle: String,
        admin: bool,
    },
    Help,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct InviteArgs {
    pub uses: Option<u32>,
    pub days: Option<u64>,
    pub label: Option<String>,
    pub url: Option<String>,
}

/// Parse the command line.
///
/// Hand-rolled rather than pulling in an argument parser: this is four
/// subcommands and four flags, and a dependency for it would be larger than the
/// thing it parses. Accepts both `--uses 5` and `--uses=5`, because people type
/// both.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, String> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Command::Serve);
    };

    match first.as_str() {
        "help" | "--help" | "-h" => Ok(Command::Help),
        "invite" => parse_invite(args).map(Command::Invite),
        verb @ ("promote" | "demote") => {
            let handle = args
                .next()
                .ok_or_else(|| format!("{verb} needs a handle, e.g. `tc-server {verb} alice`"))?;
            if let Some(extra) = args.next() {
                return Err(format!("unexpected argument after the handle: {extra}"));
            }
            Ok(Command::SetAdmin {
                // Normalized the same way registration normalizes it, so
                // `@Alice` and `alice` find the same account.
                handle: tc_core::text::normalize_handle(&handle).into_owned(),
                admin: verb == "promote",
            })
        }
        // Deliberately not "assume they meant serve": a typo like `tc-server
        // invte` would otherwise silently start a server instead of reporting
        // the mistake.
        other => Err(format!(
            "unknown command: {other}\n\nRun `tc-server help` for usage."
        )),
    }
}

fn parse_invite<I: Iterator<Item = String>>(mut args: I) -> Result<InviteArgs, String> {
    let mut out = InviteArgs::default();
    while let Some(arg) = args.next() {
        // `--flag=value` and `--flag value` are both common; support both
        // rather than making someone guess which this is.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (arg, None),
        };
        let mut value = || -> Result<String, String> {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| format!("{flag} needs a value"))
        };

        match flag.as_str() {
            "--uses" => {
                let v = value()?;
                out.uses = Some(
                    v.parse()
                        .map_err(|_| format!("--uses: {v} is not a number"))?,
                );
            }
            "--days" => {
                let v = value()?;
                out.days = Some(
                    v.parse()
                        .map_err(|_| format!("--days: {v} is not a number"))?,
                );
            }
            "--label" => out.label = Some(value()?),
            "--url" => out.url = Some(value()?),
            other => {
                return Err(format!(
                    "unknown option: {other}\n\nRun `tc-server help` for usage."
                ));
            }
        }
    }
    Ok(out)
}

/// The longest life the API allows an invite, mirrored here so the two agree.
const MAX_INVITE_DAYS: u64 = 366;

/// Where to point the printed link.
///
/// The server has no idea how it is reached — it sits behind whatever proxy the
/// operator put there — so this is a best guess that says so. A bind address of
/// `0.0.0.0` means "every interface", which is not a hostname anybody can click,
/// so it renders as `localhost` and the caller is told to set `TC_PUBLIC_URL`.
fn origin(cfg: &Config, override_url: Option<&str>) -> (String, bool) {
    if let Some(u) = override_url {
        return (u.trim_end_matches('/').to_string(), true);
    }
    if let Some(u) = cfg.public_url.as_deref().filter(|u| !u.is_empty()) {
        return (u.trim_end_matches('/').to_string(), true);
    }
    let ip = cfg.bind.ip();
    let host = if ip.is_unspecified() {
        "localhost".to_string()
    } else if ip.is_ipv6() {
        format!("[{ip}]")
    } else {
        ip.to_string()
    };
    (format!("http://{host}:{}", cfg.bind.port()), false)
}

/// Run an operator command. Returns a message to print on success.
///
/// Takes the store rather than opening one, so tests can drive it against an
/// in-memory database.
pub fn run(store: &Store, cfg: &Config, cmd: Command) -> Result<String, String> {
    match cmd {
        // Handled by the caller, which has to start a runtime for it.
        Command::Serve => Ok(String::new()),
        Command::Help => Ok(HELP.to_string()),
        Command::Invite(args) => invite(store, cfg, args),
        Command::SetAdmin { handle, admin } => set_admin(store, &handle, admin),
    }
}

fn invite(store: &Store, cfg: &Config, args: InviteArgs) -> Result<String, String> {
    let uses = args.uses.unwrap_or(1);
    let days = args.days.unwrap_or(7);
    if days > MAX_INVITE_DAYS {
        return Err(format!(
            "--days: an invite can last at most {MAX_INVITE_DAYS}"
        ));
    }
    let label = args.label.unwrap_or_default();
    if label.chars().count() > 64 {
        return Err("--label: that label is too long".into());
    }

    let now = now_ms();
    // Zero days is the caller explicitly asking for a link that never expires.
    let expires_at = (days > 0).then(|| now + days * 24 * 60 * 60 * 1000);

    let token = auth::new_session_token();
    let ids = IdGen::new(cfg.node_id);
    // On a fresh install there is genuinely nobody to credit, which is why the
    // column is nullable — see migration 009.
    let created_by = first_admin(store);

    store
        .create_invite(NewInvite {
            id: ids.next(),
            token_hash: &token.hash,
            label: &label,
            created_by,
            created_at: now,
            expires_at,
            max_uses: uses,
        })
        .map_err(|e| format!("could not create the invite: {e}"))?;

    let (origin, certain) = origin(cfg, args.url.as_deref());
    let link = format!("{origin}/#/join/{}", token.secret);

    let life = match days {
        0 => "never expires".to_string(),
        1 => "expires in a day".to_string(),
        n => format!("expires in {n} days"),
    };
    let seats = match uses {
        0 => "unlimited uses".to_string(),
        1 => "one use".to_string(),
        n => format!("{n} uses"),
    };

    let mut out = format!("Invite created — {seats}, {life}.\n\n    {link}\n");
    if !certain {
        out.push_str(
            "\nThat host is a guess from the bind address. If people reach this\n\
             server by another name, set TC_PUBLIC_URL or pass --url.\n",
        );
    }
    if store.human_count().unwrap_or(1) == 0 {
        out.push_str("\nThis workspace has no accounts yet, so whoever uses this link first\nbecomes its administrator.\n");
    }
    out.push_str("\nThe link is shown once and cannot be recovered. Mint another if it is lost.\n");
    Ok(out)
}

/// The lowest-numbered administrator, for attributing an invite to somebody.
fn first_admin(store: &Store) -> Option<Id> {
    store
        .all_users()
        .ok()?
        .into_iter()
        .find(|u| u.admin && !u.deactivated)
        .map(|u| u.id)
}

fn set_admin(store: &Store, handle: &str, admin: bool) -> Result<String, String> {
    let user = store
        .user_by_handle(handle)
        .map_err(|e| format!("could not read the account: {e}"))?
        .ok_or_else(|| format!("no account with the handle {handle}"))?;

    if user.bot {
        // Bots cannot sign in, so an administrator bot is a privilege nobody
        // can exercise and everybody can forget about.
        return Err(format!("{handle} is a bot; bots cannot be administrators"));
    }
    if user.admin == admin {
        return Ok(format!(
            "{handle} is already {}.",
            if admin {
                "an administrator"
            } else {
                "not an administrator"
            }
        ));
    }

    store
        .set_admin(user.id, admin)
        .map_err(|e| format!("could not update the account: {e}"))?;

    let mut out = if admin {
        format!("{handle} is now an administrator.")
    } else {
        format!("{handle} is no longer an administrator.")
    };
    // Demoting the last one leaves a workspace nobody can administer through
    // the API. Allowed — this console is how you get back out — but said aloud.
    if !admin && store.admin_count().unwrap_or(1) == 0 {
        out.push_str("\n\nThat was the last administrator. Run `tc-server promote <handle>` to appoint another.");
    }
    if user.deactivated {
        out.push_str("\n\nNote: this account is deactivated and cannot sign in until an administrator restores it.");
    }
    Ok(out)
}

/// Report, at startup, a workspace nobody can get into.
///
/// The whole failure mode this module exists for is silent: the server starts,
/// serves a login page, and refuses every credential because there are none.
/// Saying so — with the command that fixes it — is what keeps the fix
/// discoverable without reading the manual.
pub fn warn_if_unreachable(store: &Store, cfg: &Config) {
    let humans = store.human_count().unwrap_or(1);
    let admins = store.admin_count().unwrap_or(1);

    if humans == 0 && !cfg.open_registration {
        tracing::warn!(
            "no accounts yet and registration is closed — nobody can sign in. \
             Run `tc-server invite` to mint a link; whoever uses it first \
             becomes the administrator."
        );
    } else if admins == 0 && humans > 0 {
        tracing::warn!(
            "this workspace has no active administrator. \
             Run `tc-server promote <handle>` to appoint one."
        );
    }
}

/// Open the database for an operator command.
///
/// Separate from the server's own startup because it must not create the blob
/// directory or bind a port — and because it has to work while the server is
/// running. SQLite's WAL mode allows exactly that: a second process writes
/// concurrently, and `busy_timeout` absorbs the contention.
pub fn open_store(path: &Path) -> Result<Store, String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    Store::open(path).map_err(|e| format!("could not open {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn cfg() -> Config {
        Config::default()
    }

    // -- Parsing ------------------------------------------------------------

    #[test]
    fn no_arguments_runs_the_server() {
        assert_eq!(parse(args(&[])).unwrap(), Command::Serve);
    }

    #[test]
    fn an_unknown_command_is_an_error_not_a_silent_serve() {
        // A typo must not start a server: the operator would see it come up and
        // assume the command they typed had worked.
        let err = parse(args(&["invte"])).unwrap_err();
        assert!(err.contains("unknown command"), "{err}");
        assert!(err.contains("help"), "the error should say where to look");
    }

    #[test]
    fn invite_takes_both_flag_spellings() {
        let spaced = parse(args(&["invite", "--uses", "5", "--days", "2"])).unwrap();
        let joined = parse(args(&["invite", "--uses=5", "--days=2"])).unwrap();
        assert_eq!(spaced, joined);
        assert_eq!(
            spaced,
            Command::Invite(InviteArgs {
                uses: Some(5),
                days: Some(2),
                ..Default::default()
            })
        );
    }

    #[test]
    fn invite_defaults_to_nothing_and_lets_the_command_decide() {
        assert_eq!(
            parse(args(&["invite"])).unwrap(),
            Command::Invite(InviteArgs::default())
        );
    }

    #[test]
    fn a_label_may_contain_spaces_and_an_equals_sign() {
        let cmd = parse(args(&["invite", "--label", "design = contractors"])).unwrap();
        assert_eq!(
            cmd,
            Command::Invite(InviteArgs {
                label: Some("design = contractors".into()),
                ..Default::default()
            })
        );
    }

    #[test]
    fn malformed_invite_options_are_reported() {
        for bad in [
            args(&["invite", "--uses"]),
            args(&["invite", "--uses", "lots"]),
            args(&["invite", "--days", "soon"]),
            args(&["invite", "--nonsense"]),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn promote_and_demote_normalize_the_handle() {
        // `@Alice` and `alice` are the same account, exactly as at registration.
        assert_eq!(
            parse(args(&["promote", "@Alice"])).unwrap(),
            Command::SetAdmin {
                handle: "alice".into(),
                admin: true
            }
        );
        assert_eq!(
            parse(args(&["demote", "alice"])).unwrap(),
            Command::SetAdmin {
                handle: "alice".into(),
                admin: false
            }
        );
    }

    #[test]
    fn promote_needs_exactly_one_handle() {
        assert!(parse(args(&["promote"])).is_err());
        assert!(parse(args(&["promote", "alice", "bob"])).is_err());
    }

    #[test]
    fn help_is_reachable_the_three_ways_people_try() {
        for spelling in ["help", "--help", "-h"] {
            assert_eq!(parse(args(&[spelling])).unwrap(), Command::Help);
        }
    }

    // -- The printed origin --------------------------------------------------

    #[test]
    fn an_explicit_url_wins_and_loses_its_trailing_slash() {
        let (o, certain) = origin(&cfg(), Some("https://chat.example.com/"));
        assert_eq!(o, "https://chat.example.com");
        assert!(certain);
    }

    #[test]
    fn the_configured_public_url_is_used_when_no_flag_is_given() {
        let c = Config {
            public_url: Some("https://chat.example.com".into()),
            ..Config::default()
        };
        let (o, certain) = origin(&c, None);
        assert_eq!(o, "https://chat.example.com");
        assert!(certain);
    }

    #[test]
    fn a_wildcard_bind_becomes_localhost_and_admits_it_is_a_guess() {
        // `0.0.0.0` means "every interface"; it is not a host anyone can click.
        let c = Config {
            bind: "0.0.0.0:8080".parse().unwrap(),
            ..Config::default()
        };
        let (o, certain) = origin(&c, None);
        assert_eq!(o, "http://localhost:8080");
        assert!(!certain, "the caller must be told this is a guess");
    }

    #[test]
    fn an_ipv6_bind_is_bracketed_so_the_url_parses() {
        let c = Config {
            bind: "[::1]:8080".parse().unwrap(),
            ..Config::default()
        };
        assert_eq!(origin(&c, None).0, "http://[::1]:8080");
    }

    // -- invite --------------------------------------------------------------

    /// Mint an invite and return the token from the printed link.
    fn token_from(output: &str) -> String {
        output
            .split("/#/join/")
            .nth(1)
            .expect("the output must contain a link")
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    }

    #[test]
    fn an_invite_from_the_console_admits_the_first_administrator() {
        // The whole point: a workspace with registration closed and nobody in
        // it can still be entered, without ever opening registration.
        let store = Store::open_in_memory().unwrap();
        let c = Config {
            open_registration: false,
            ..Config::default()
        };

        let out = run(&store, &c, Command::Invite(InviteArgs::default())).unwrap();
        assert!(out.contains("becomes its administrator"), "{out}");

        let secret = token_from(&out);
        let hash = auth::token_hash(&secret);
        assert!(store.invite_is_live(&hash, now_ms()).unwrap());

        // Redeeming it goes through the ordinary path, so the first human
        // becomes the administrator with no special case anywhere.
        let user = store
            .create_user_via_invite(Id(999), "alice", "Alice", "phc", &hash, now_ms())
            .unwrap();
        assert!(user.admin);
        assert_eq!(store.admin_count().unwrap(), 1);

        // ...and it was single-use, so the link is spent.
        assert!(!store.invite_is_live(&hash, now_ms()).unwrap());
    }

    #[test]
    fn the_secret_appears_once_and_is_not_recoverable_from_the_database() {
        let store = Store::open_in_memory().unwrap();
        let out = run(&store, &cfg(), Command::Invite(InviteArgs::default())).unwrap();
        let secret = token_from(&out);

        let listed = store.invites().unwrap();
        assert_eq!(listed.len(), 1);
        // Nothing stored can reconstruct the link.
        let dump = format!("{listed:?}");
        assert!(!dump.contains(&secret), "the secret must not be in the row");
    }

    #[test]
    fn invite_options_reach_the_stored_row() {
        let store = Store::open_in_memory().unwrap();
        let out = run(
            &store,
            &cfg(),
            Command::Invite(InviteArgs {
                uses: Some(0),
                days: Some(0),
                label: Some("contractors".into()),
                url: Some("https://chat.example.com".into()),
            }),
        )
        .unwrap();

        assert!(out.contains("https://chat.example.com/#/join/"), "{out}");
        assert!(out.contains("unlimited uses"), "{out}");
        assert!(out.contains("never expires"), "{out}");

        let inv = &store.invites().unwrap()[0];
        assert_eq!(inv.max_uses, 0);
        assert_eq!(inv.expires_at, None);
        assert_eq!(inv.label, "contractors");
    }

    #[test]
    fn an_invite_cannot_outlive_the_limit_the_api_enforces() {
        // The two surfaces mint the same rows, so they must agree on the rules.
        let store = Store::open_in_memory().unwrap();
        let err = run(
            &store,
            &cfg(),
            Command::Invite(InviteArgs {
                days: Some(400),
                ..Default::default()
            }),
        )
        .unwrap_err();
        assert!(err.contains("at most"), "{err}");
        assert!(store.invites().unwrap().is_empty(), "nothing was written");
    }

    #[test]
    fn an_invite_is_attributed_to_an_administrator_when_there_is_one() {
        let store = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        let alice = store.create_user(g.next(), "alice", "Alice", "h").unwrap();
        assert!(alice.admin);

        run(&store, &cfg(), Command::Invite(InviteArgs::default())).unwrap();
        assert_eq!(store.invites().unwrap()[0].created_by, Some(alice.id));
    }

    #[test]
    fn an_invite_on_an_empty_workspace_is_attributed_to_nobody() {
        // The case the whole console exists for. There is nobody to credit, so
        // the column is NULL rather than pointing at an invented account.
        let store = Store::open_in_memory().unwrap();
        run(&store, &cfg(), Command::Invite(InviteArgs::default())).unwrap();
        assert_eq!(store.invites().unwrap()[0].created_by, None);
    }

    // -- promote / demote ----------------------------------------------------

    #[test]
    fn promote_grants_administrator_and_demote_takes_it_back() {
        let store = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        store.create_user(g.next(), "alice", "Alice", "h").unwrap();
        let bob = store.create_user(g.next(), "bob", "Bob", "h").unwrap();
        assert!(!bob.admin);

        let out = run(
            &store,
            &cfg(),
            Command::SetAdmin {
                handle: "bob".into(),
                admin: true,
            },
        )
        .unwrap();
        assert!(out.contains("now an administrator"), "{out}");
        assert!(store.user(bob.id).unwrap().admin);

        let out = run(
            &store,
            &cfg(),
            Command::SetAdmin {
                handle: "bob".into(),
                admin: false,
            },
        )
        .unwrap();
        assert!(out.contains("no longer"), "{out}");
        assert!(!store.user(bob.id).unwrap().admin);
    }

    #[test]
    fn promoting_someone_who_already_is_says_so_without_failing() {
        // Re-running a provisioning script must not error.
        let store = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        store.create_user(g.next(), "alice", "Alice", "h").unwrap();
        let out = run(
            &store,
            &cfg(),
            Command::SetAdmin {
                handle: "alice".into(),
                admin: true,
            },
        )
        .unwrap();
        assert!(out.contains("already"), "{out}");
    }

    #[test]
    fn an_unknown_handle_is_an_error() {
        let store = Store::open_in_memory().unwrap();
        let err = run(
            &store,
            &cfg(),
            Command::SetAdmin {
                handle: "nobody".into(),
                admin: true,
            },
        )
        .unwrap_err();
        assert!(err.contains("no account"), "{err}");
    }

    #[test]
    fn a_bot_cannot_be_promoted() {
        // A bot cannot sign in, so an administrator bot is a privilege nobody
        // can exercise and everybody can forget is there.
        let store = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        store.create_bot(g.next(), "deploybot", "Deploy").unwrap();
        let err = run(
            &store,
            &cfg(),
            Command::SetAdmin {
                handle: "deploybot".into(),
                admin: true,
            },
        )
        .unwrap_err();
        assert!(err.contains("bot"), "{err}");
    }

    #[test]
    fn a_deactivated_account_can_be_promoted_but_the_fact_is_reported() {
        // Legitimate while restoring a workspace, and misleading in silence:
        // the account still cannot sign in until it is reactivated.
        let store = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        store.create_user(g.next(), "alice", "Alice", "h").unwrap();
        let bob = store.create_user(g.next(), "bob", "Bob", "h").unwrap();
        store.set_deactivated(bob.id, true).unwrap();

        let out = run(
            &store,
            &cfg(),
            Command::SetAdmin {
                handle: "bob".into(),
                admin: true,
            },
        )
        .unwrap();
        assert!(out.contains("now an administrator"), "{out}");
        assert!(
            out.contains("deactivated"),
            "the caveat must be stated: {out}"
        );
    }

    #[test]
    fn demoting_the_last_administrator_is_allowed_but_announced() {
        // The API refuses this, because through the API it is unrecoverable.
        // Here it is recoverable — this console is the way back — so it is
        // permitted and called out rather than blocked.
        let store = Store::open_in_memory().unwrap();
        let g = IdGen::new(1);
        store.create_user(g.next(), "alice", "Alice", "h").unwrap();

        let out = run(
            &store,
            &cfg(),
            Command::SetAdmin {
                handle: "alice".into(),
                admin: false,
            },
        )
        .unwrap();
        assert_eq!(store.admin_count().unwrap(), 0);
        assert!(out.contains("last administrator"), "{out}");
        assert!(out.contains("promote"), "and it names the way back: {out}");
    }

    #[test]
    fn help_names_every_command_it_supports() {
        // A help text that has drifted from the parser is worse than none.
        let store = Store::open_in_memory().unwrap();
        let out = run(&store, &cfg(), Command::Help).unwrap();
        for (verb, extra) in [
            ("invite", None),
            ("promote", Some("alice")),
            ("demote", Some("alice")),
        ] {
            assert!(out.contains(verb), "help omits {verb}");
            let mut argv = vec![verb];
            argv.extend(extra);
            assert!(
                parse(args(&argv)).is_ok(),
                "{verb} is documented but does not parse"
            );
        }
        for flag in ["--uses", "--days", "--label", "--url"] {
            assert!(out.contains(flag), "help omits {flag}");
        }
    }
}
