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

- **T0 — the switcher.** `add`, `list`, `switch`, `status`, `remove`.
- **T1 — quota.** Live 5h / 7d / per-model numbers on `list`, with token refresh
  and a short-lived cache.

Auto-switching, session mode (`run`), directory mappings, import/export and the
TUI are **not** implemented and are not planned. If you want those, use the
Python tool — it reads the same store, so you can have both.

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
```

`--json` on `list`, `status` and `switch` emits a single object on stdout.
`--refresh` bypasses the quota cache, `--offline` never touches the network, and
`--no-usage` skips quota entirely.

Re-running `add` while logged in as an account you already have updates that
slot in place — that is how you recover an expired login.

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
