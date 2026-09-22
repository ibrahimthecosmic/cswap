# cswap

Multi-account switcher for Claude Code, in Rust. A port of the switching core of
[claude-swap](https://github.com/realiti4/claude-swap) — same on-disk store, no
Python, one dependency.

```
$ cswap
○ 1  ibrahim@example.com
    5h     ░░░░░░░░░░░░░░   0%  resets in 4h 27m  (Aug 27 03:19 UTC)
    7d     █░░░░░░░░░░░░░   3%  resets in 6d 3h  (Sep 2 01:59 UTC)

● 2  tech@example.com  (active)
    5h     █░░░░░░░░░░░░░   4%  resets in 4h 27m  (Aug 27 03:19 UTC)
    7d     ████████░░░░░░  58%  resets in 1d 7h  (Aug 28 05:59 UTC)
    Fable  ██░░░░░░░░░░░░  14%  resets in 1d 7h  (Aug 28 05:59 UTC)
```

## Scope

Two tiers, and no more:

- **T0 — the switcher.** `add`, `list`, `switch`, `status`, `remove`, and
  `export` / `import` to carry accounts between machines.
- **T1 — quota.** Live 5h / 7d / per-model numbers on `list`, with token refresh
  and a short-lived cache.
- **Borrowing.** `borrow` / `lease` / `token`: use a login that lives on another
  of your machines, over SSH, without ever copying its refresh token.

Auto-switching, session mode (`run`), directory mappings and the TUI are **not**
implemented and are not planned. If you want those, use the Python tool — it
reads the same store, so you can have both.

## Install

Linux (x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/ibrahimthecosmic/cswap/main/install.sh | sh
```

Windows (x86_64), in PowerShell:

```powershell
irm https://raw.githubusercontent.com/ibrahimthecosmic/cswap/main/install.ps1 | iex
```

Both drop a single binary — `~/.local/bin/cswap`, or `%LOCALAPPDATA%\Programs\cswap` on
Windows — and take `CSWAP_INSTALL_DIR` and `CSWAP_VERSION` to override either.
The Linux build is statically linked against musl, so it has no glibc floor.

From source, on any platform:

```sh
cargo build --release                          # target/release/cswap, ~1.6 MB
cargo build --release --no-default-features    # ~410 KB, zero dependencies, no quota
```

Updating:

```sh
cswap upgrade            # fetch the latest release and replace this binary
cswap upgrade --check    # just report what is available
```

## Usage

```sh
cswap                     # list every account with its quota (default command)
cswap switch              # rotate to the next account
cswap switch 2            # or by slot, email, or alias
cswap add                 # store the account you are logged in as right now
cswap add --slot 3 --alias work
cswap status
cswap remove 3
cswap upgrade

cswap borrow work --from me@desktop                  # use desktop's "work" login here
cswap lease                                          # keep borrowed logins current

cswap export you@example.com --output ./creds.json   # one account
cswap export --output ./accounts.json                # all of them
cswap import ./creds.json                            # on the other machine
```

`--json` on `list`, `status`, `switch`, `export` and `import` emits a single
object on stdout.
`--refresh` bypasses the quota cache, `--offline` never touches the network, and
`--no-usage` skips quota entirely.

Re-running `add` while logged in as an account you already have updates that
slot in place — that is how you recover an expired login.

### Moving accounts between machines

`export` writes one account, or every account, as a single JSON file; `import`
reads it back into another machine's store. `-` stands for stdout and stdin, so
the two pipe together:

```sh
cswap export work --output - | ssh other-box cswap import -
```

What travels is the login and nothing else: the credential, and the
`oauthAccount` block that names it. The stored config snapshot — your whole
`~/.claude.json`, projects and history included — stays where it is; `switch`
only ever reads that one key out of it.

`import` places each account the way `add` does: an account you already have
updates its existing slot, anything new takes the next free one. `--slot` and
`--alias` override that for a single-account file, and `--force` is required to
overwrite a slot that holds someone else or to move an account to a different
slot. Importing does not log you in — run `cswap switch` after it.

**The file holds live OAuth tokens in plaintext.** It is written owner-only
(0600), `export` will not silently overwrite an existing file, and the sensible
thing is to delete it once it has been imported.

**An export is a point-in-time copy of a single-use token lineage.** Refreshing
rotates the refresh token and invalidates the one before it, so once either
machine refreshes, the other machine's copy stops working — it reports the login
as expired while the machine that refreshed carries on fine. Export immediately
before you import, and expect to repeat it rather than treating the two stores
as synchronised. To use one account on two machines at the same time, log in
separately on each (`claude`, then `cswap add`): each login gets its own lineage,
and the two do not fight.

Exporting the account you are currently logged in as takes the credential Claude
Code is using right now, not the slot's last snapshot — Claude Code refreshes in
place, and a snapshot older than that is already spent. The snapshot is refiled
on the way past, so the store stops being stale too.

### Borrowing a login from another machine

Log in once, on one machine, and use that login from all of your others:

```sh
# on the laptop; "desktop" is any SSH destination that has cswap installed
cswap borrow work --from me@desktop    # slot, email or alias as desktop knows it
cswap switch work
cswap lease                            # leave running; Ctrl-C to stop
```

The machine that did the browser login is the **owner**. It keeps the refresh
token and is the only machine that ever refreshes. Every other machine gets the
access token alone — no refresh token, nothing that could spend the lineage —
so there is no way for two machines to fight over it, which is what breaks a
plain `export`/`import` copy.

The catch is that a refresh on the owner **revokes the previous access token
immediately**. Whether cswap or Claude Code on the owner does the refresh, the
token every borrower holds stops working at that moment. So `cswap lease` holds
one `ssh <owner> cswap token <account> --watch` open per borrowed account; the
owner checks for a new token every few seconds, refreshes shortly before expiry,
and pushes each new one down the pipe. A borrower's Claude Code picks the new
token up on its next message — a request in flight when the owner refreshes can
fail with a 401, and simply retrying it works.

- Without `lease` running, `cswap switch` onto a borrowed account fetches a fresh
  token first. That is enough for short sessions; a long one outlives the token.
- The owner must be reachable. While it is off or asleep, borrowers keep working
  until the owner's next refresh would have happened, and then they stop.
- The transport is plain SSH (`BatchMode`, so keys or an agent — no password
  prompts). It works between Linux and Windows in either direction; a Windows
  owner needs its OpenSSH Server enabled. `CSWAP_SSH` overrides the ssh binary.
  If `cswap` is not on the owner's non-interactive `PATH`, pass
  `--remote-bin /path/to/cswap` to `borrow`.
- A borrowed slot shows as `borrowed from <host>` in `list`. It cannot be
  exported or lent on, and cswap never tries to refresh it. Logging in as the
  account locally and running `cswap add` makes the slot this machine's own
  again.
- `ssh desktop cswap token work | cswap borrow -` does a one-off borrow without
  recording where it came from — nothing can renew it afterwards.
- All borrowers draw on the owner account's quota, and this is for your own
  machines. Handing a login to someone else is account sharing.

## Dependencies

One, and only for T1: [`ureq`](https://docs.rs/ureq) (with `rustls`) for HTTPS to
the two Anthropic OAuth endpoints. JSON, base64, file locking and date
arithmetic are all in-tree — see `src/json.rs`, `src/b64.rs`, `src/lock.rs`,
`src/timefmt.rs`. `--no-default-features` drops `ureq` and builds a switcher with
no dependencies and no network code at all.

TLS trust comes from the `webpki-roots` bundle, not the system trust store, so a
TLS-intercepting corporate proxy will fail the quota calls. `switch` is unaffected
— it makes no network calls.

## How it works

**Switching** is three writes under two locks:

1. The live credential is filed under the outgoing slot, along with the
   `~/.claude.json` snapshot that names it.
2. The target slot's credential is written to Claude Code's live store, holding
   Claude Code's own `.oauth_refresh.lock` and `~/.claude.lock`.
3. `oauthAccount` in `~/.claude.json` is replaced — and *only* `oauthAccount`,
   under `~/.claude.json.lock`, preserving every other key and its position.

The locks are npm `proper-lockfile` directory locks, the protocol Claude Code
uses. Holding them closes the one real race with a running Claude Code: its token
refresh is read → network → write, all under the lock, so a swap landing inside
that window would be overwritten by the refreshed *old* account's token, and the
refresh token just filed away would already be spent.

Not everything in a credential belongs to the account. `mcpOAuth` and friends are
machine-shared OAuth state that rotates independently of any slot, so on
activation the live copy wins — including by being absent. Everything else,
`trustedDeviceToken` included, travels with the slot.

**Quota** reads each slot's access token and calls `/api/oauth/usage`. An expired
token is refreshed first, and that is the one dangerous write in the program:
refresh tokens are single-use, so the rotated credential is persisted before it
is used for anything, behind a per-slot `flock` gate so two processes never spend
the same token. For the active slot the result is compare-and-swapped into the
live store — if Claude Code refreshed first, its generation wins and ours is
discarded.

## Store layout

Shared with claude-swap, byte-for-byte:

```
~/.local/share/claude-swap/            # macOS/Windows: ~/.claude-swap-backup
  sequence.json                        registry, rotation order, active slot
  configs/.claude-config-{n}-{email}.json
  credentials/.creds-{n}-{email}.enc   base64 of the slot's credential JSON
  cache/usage-cswap-rs.json            this port's quota cache (its own file)
  cluster-cswap-rs.json                which slots are borrowed, and from where
```

`XDG_DATA_HOME` and `CLAUDE_CONFIG_DIR` are both honoured.

## Caveats

- Reset times are shown as a countdown plus the **UTC** instant. Rendering local
  wall-clock time needs the platform timezone database, which is the one thing
  worth a dependency that this does not take.
- On macOS the credential is read and written through the `security` CLI, and
  Claude Code caches Keychain reads for ~30s, so a switch takes a moment to land.
  On Linux and Windows it applies on your next message.
- Windows has no `flock`; the store lock there excludes other `cswap` processes
  but does not interlock with claude-swap's `msvcrt` byte-range lock.

## Licence

MIT, matching the project it ports.
