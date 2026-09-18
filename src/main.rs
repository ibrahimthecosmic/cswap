//! cswap — multi-account switcher for Claude Code.
//!
//! A port of the switching core of https://github.com/realiti4/claude-swap,
//! reading and writing that tool's own on-disk store so the two can share a
//! machine. Scope is deliberately two tiers: T0, the switcher (`add`, `list`,
//! `switch`, `status`, `remove`), and T1, live quota numbers on `list`.
//! Auto-switching, session mode and the TUI are out of scope; `export` and
//! `import` move accounts between machines without one.

mod b64;
mod fsx;
mod json;
mod live;
mod lock;
mod model;
mod paths;
mod portable;
mod store;
mod timefmt;

#[cfg(feature = "usage")]
mod api;
#[cfg(feature = "usage")]
mod upgrade;
#[cfg(feature = "usage")]
mod usage;

mod cmd;
mod render;

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    reset_sigpipe();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("cswap: {message}");
            ExitCode::from(1)
        }
    }
}

/// Rust starts with `SIGPIPE` ignored, which turns `cswap list | head` into a
/// panic on a broken pipe instead of a quiet exit. Restore the default.
#[cfg(unix)]
fn reset_sigpipe() {
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;
    // SAFETY: setting a signal disposition to the default is always valid.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let mut flags = Flags::default();
    let mut positional: Vec<&str> = Vec::new();

    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => flags.json = true,
            "--refresh" => flags.refresh = true,
            "--offline" => flags.offline = true,
            "--no-usage" => flags.no_usage = true,
            "--force" | "-f" => flags.force = true,
            "--yes" | "-y" => flags.yes = true,
            "--check" => flags.check = true,
            "--slot" => {
                let value = it.next().ok_or("--slot needs a number")?;
                flags.slot = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--slot: '{value}' is not a number"))?,
                );
            }
            "--alias" => flags.alias = Some(it.next().ok_or("--alias needs a name")?.clone()),
            "--output" | "-o" => {
                flags.output = Some(it.next().ok_or("--output needs a path")?.clone())
            }
            "--help" | "-h" | "help" => {
                print!("{}", help_text());
                return Ok(ExitCode::SUCCESS);
            }
            "--version" | "-V" => {
                println!("cswap {VERSION}");
                return Ok(ExitCode::SUCCESS);
            }
            // A bare '-' is stdin/stdout for import/export, not a flag.
            other if other.starts_with('-') && other != "-" => {
                return Err(format!("unknown flag '{other}'"))
            }
            other => positional.push(other),
        }
    }

    let (command, rest) = positional
        .split_first()
        .map_or(("list", &[][..]), |(c, r)| (*c, r));
    match command {
        "list" | "ls" => cmd::list(&flags),
        "status" => cmd::status(&flags),
        "switch" => cmd::switch(rest.first().copied(), &flags),
        "add" => cmd::add(&flags),
        "export" => cmd::export(rest.first().copied(), &flags),
        "import" => {
            let source = rest
                .first()
                .copied()
                .ok_or("import needs a file (or `-` for stdin)")?;
            cmd::import(source, &flags)
        }
        "upgrade" => cmd::upgrade(&flags),
        "remove" | "rm" => {
            let target = rest
                .first()
                .copied()
                .ok_or("remove needs an account (slot, email or alias)")?;
            cmd::remove(target, &flags)
        }
        other => Err(format!("unknown command '{other}' — try `cswap help`")),
    }
}

#[derive(Default)]
pub struct Flags {
    pub json: bool,
    pub refresh: bool,
    pub offline: bool,
    pub no_usage: bool,
    pub force: bool,
    pub yes: bool,
    pub check: bool,
    pub slot: Option<i64>,
    pub alias: Option<String>,
    pub output: Option<String>,
}

fn help_text() -> String {
    format!(
        "\
cswap {VERSION} — multi-account switcher for Claude Code

USAGE
  cswap [command] [options]

COMMANDS
  list                 Every account with its 5h / 7d quota (default)
  status               The account Claude Code is logged in as
  switch [<account>]   Switch accounts; bare rotates to the next one
  add                  Store the account you are logged in as right now
  remove <account>     Forget an account and delete its stored login
  export [<account>]   Write accounts to a portable file; bare exports all
  import <file>        Read accounts back out of one (`-` reads stdin)
  upgrade              Replace this binary with the latest GitHub release

  <account> is a slot number, an email, or an alias.

OPTIONS
  --json               Machine-readable output on stdout
  --refresh            Ignore cached quota numbers and re-fetch
  --offline            Never touch the network; show cached numbers only
  --no-usage           Skip quota entirely (list/status)
  --slot <n>           add/import: which slot to write (default: reuse or next free)
  --alias <name>       add/import: give the account a short name
  --output, -o <file>  export: where to write it (`-` writes stdout)
  --force              switch: re-activate even if it is already active
                       export: overwrite the output file
                       import: overwrite or move an occupied slot
  --yes                remove: do not ask for confirmation
  --check              upgrade: report what is available, install nothing
  --help, --version

NOTES
  An export holds live login tokens in plaintext. It is written owner-only;
  keep it off shared storage and delete it once it has been imported.

  Switching does not usually need a restart. On Linux and Windows Claude Code
  re-reads its credentials file when it changes, so the new account applies on
  your next message. On macOS the Keychain read is cached for ~30s.

  Data lives in {}
",
        paths::backup_root().display()
    )
}
