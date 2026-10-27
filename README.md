# UsageHub

![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

UsageHub is a fork of [Claude Code Usage Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor) (MIT, Copyright (C) 2026 Code Zeno Pty Ltd), renamed so it can run side-by-side with the upstream app. It keeps the lightweight Windows taskbar bar for monitoring Claude Code usage limits and reset times, with support for Codex, Google Antigravity, OpenCode Go, Cursor, Grok Build, and GitHub Copilot, built-in themes, a visual Theme Studio, and an account manager for Claude Code and Codex.

**Fork repository:** [rostikowb/UsageHub](https://github.com/rostikowb/UsageHub), branch `usagehub`. The CodeZeno repository linked above is upstream, not the publication target for this fork.

Fork differences at a glance:

- Own identity: `%APPDATA%\UsageHub` settings, `Local\UsageHub*` mutexes and events, the `UsageHub` window class, the `UsageHub` Run key, `%TEMP%\usagehub.log` diagnostics, and the `usagehub.exe` binary. Nothing is shared with an upstream install.
- Local-first account manager: one executable runs the taskbar bar and opens its dashboard or Theme Studio on demand. A new Claude account is a Desktop user-data directory with its own nested `claude-config`; separate Claude windows launch from the installed app and open only when requested. Credentials, config, and history stay isolated per account — the deliberate shared state is the skills library.
- Fork-owned updater: portable builds check [UsageHub releases](https://github.com/rostikowb/UsageHub/releases), accept only `usagehub.exe` from this fork, and verify its size and SHA-256 before replacement. They never install an upstream monitor build.
- Exact usage data only: the limits and reset times shown always come from each provider's own service (for Claude, the original usageBar server poller). Automatic monitoring never runs `claude -p` or another model/CLI probe, and there are no switcher estimates, plan-usage-history inference, or manual reset overrides.
- GPU dashboard: the existing Rust egui interface now prefers native hardware Direct3D 11, with WARP **software** and eframe/OpenGL fallbacks. Settings run on demand in a separate process; closing them leaves only the original taskbar bar. See [dashboard rendering](docs/dashboard-rendering.md) for renderer selection, bounded timing diagnostics and verification.

See the [user guide](USER_GUIDE.md) for theme customisation and everyday settings,
or the [changelog](CHANGELOG.md) for version history and notable changes.

## Maintainer handoff

Start with [docs/HANDOFF.md](docs/HANDOFF.md) for the implemented architecture, local project/install paths, production migration state, safe diagnostics, verification evidence, limitations, and next-session checklist.

- [Windows build setup](docs/build-windows.md): pinned Rust/LLVM toolchain and build commands.
- [Migration and rollback](docs/migration.md): account adoption, shared skills, legacy sync retirement, encrypted recovery.
- [Historical implementation contract](docs/implementation-contract.md): original design notes, not an unfinished-work checklist or authoritative Rust API reference.

![UsageHub running in the Windows taskbar](.github/animation.gif)

## Features

- Displays current usage and time remaining until each limit resets
- Counts usage up from zero or down from the full allowance, whichever you prefer
- Supports Claude Code, Codex, Google Antigravity, OpenCode Go, Cursor, Grok Build, and GitHub Copilot
- Manages multiple Claude Code and Codex accounts: add, import, rename, open, focus, pin, stop, and per-account CLI environments
- Shows one taskbar slot per enabled and pinned account with a width budget and overflow popup
- Stores encrypted seven-day undo snapshots before deletion and refuses restoring over live data
- Cleans only proven owned data through previews and typed confirmation, with no forensic erasure
- Shares a skills library between accounts without sharing credentials, config, or history
- Lives in the Windows taskbar with quick controls in the system tray
- Supports multiple monitors and Windows startup
- Includes configurable refresh intervals, providers, languages, and display options
- Provides built-in themes and a visual Theme Studio for custom layouts
- Exposes a noninteractive `--manage` JSON command surface for scripts and diagnostics
- Collects no analytics or telemetry

## Requirements

- Windows 10 or Windows 11
- At least one supported provider installed and signed in

Claude Code credentials are read from stored logins: the credentials file written by a CLI sign-in, the Claude desktop app's own token cache, a WSL installation, or a confirmed Windows Credential Manager target belonging to a managed account. UsageHub does not launch `claude` to probe or refresh tokens. Other providers are optional and can be enabled independently from the dashboard.

## Installation

Download `usagehub.exe` from [the latest UsageHub release](https://github.com/rostikowb/UsageHub/releases/latest) and run it from a user-writable folder, or build from source below. An upstream `ClaudeCodeUsageMonitor` install may stay alongside it: settings, mutexes, startup entries, and logs are separate. The upstream installer is not a UsageHub update channel.

## Usage

Start the monitor:

```powershell
usagehub
```

Open the settings dashboard directly:

```powershell
usagehub --dashboard
```

Use the dashboard to select providers, change the refresh interval, choose a display, enable startup, or customize the widget. **Settings > Display > Usage direction** switches the default theme and other themes that support this setting between showing what has been used and what is left, with Used as the default. Selecting Remaining makes a fresh limit read 100% and drain as you work.

At Windows startup the app starts the taskbar bar only; the **Settings** dashboard and Theme Studio open on demand from the tray menu or with `usagehub --dashboard` / `usagehub --studio`. Claude account windows likewise open only when you select an account, and each launch uses that account's own data directory.

Scripted management uses the same executable. `usagehub --manage help` lists the command surface (`list`, `create`, `import`, `rename`, `open`, `focus`, `status`, `stop`, `pin`, `cli`, `poll`, `snapshot`, `cleanup`, `backup`, `skills`, `migration`, `doctor`). Successful commands write `{"ok":true,"data":…}` to stdout, failures write an error envelope to stderr, and the exit codes are `0` (ok), `2` (usage error), and `3` (operation error), so output can be redirected from a shell.

Theme authors can opt in with `.display` bindings, including `{claude.session.display:usage_line}` and `{claude.session.display:usage_badge}`. Existing `.percentage`, `.remaining`, and unsuffixed usage summaries keep their meaning; warning thresholds should continue to use `.percentage`.

In the default theme, left-click a provider tray icon to show or hide the widget and right-click it to open the menu.

## Provider setup

| Provider | Setup |
| --- | --- |
| Claude Code | Sign in with the Claude Code CLI or desktop app. The stored Windows or WSL login is read directly; a managed account may also use its confirmed Windows Credential Manager targets. |
| Codex | Install and sign in to the Codex CLI, then enable Codex in **Providers**. |
| Google Antigravity | Sign in to Antigravity, then enable it in **Providers**. |
| OpenCode Go | Connect an OpenCode Go account, configure the credentials described below, then enable OpenCode in **Providers**. |
| Cursor | Sign in to Cursor, then enable it in **Providers**. The local session is detected automatically. |
| Grok Build | Run `grok login` in the Grok Build CLI, then enable Grok in **Providers**. The signed-in session is detected automatically. |
| GitHub Copilot | Sign in with the Copilot CLI (`copilot`, then `/login`) or the GitHub CLI (`gh auth login`), then enable Copilot in **Providers**. The stored login is detected automatically. |

For OpenCode Go, set `OPENCODE_GO_WORKSPACE_ID` and `OPENCODE_GO_AUTH_COOKIE`, or create `%APPDATA%\opencode-go\config.json`:

```json
{
  "workspaceId": "wrk_01...",
  "authCookie": "__Host-console_session=your-session-cookie-value"
}
```

The workspace ID is part of the OpenCode Go console URL: `https://opencode.ai/console/<workspaceId>/go`. Copy the `__Host-console_session` cookie from an authenticated `opencode.ai` browser session, including its name as shown above. A full Cookie header containing `__Host-console_session` or the legacy `auth` cookie is also accepted unchanged; no empty `auth=;` prefix is needed. Bare legacy `auth` cookie values remain supported. These formats work for both `authCookie` and `OPENCODE_GO_CONFIG_FILE`. Set `OPENCODE_GO_CONFIG_FILE` to use a different config path. The monitor reads usage from the console JSON API using this workspace ID and cookie.

For Cursor, `CURSOR_SESSION_TOKEN` can override the automatically detected local session.

Grok Build usage comes from the session the CLI stores in `%USERPROFILE%\.grok\auth.json`, read through the same billing endpoint as the CLI's own `/usage` panel. Set `GROK_HOME` if the CLI keeps its home directory elsewhere. Only xAI sign-in entries are used; corporate identity-provider tokens and stored API keys are excluded. A bare `XAI_API_KEY` is not enough: the shared weekly allowance is only readable with a signed-in session. Grok reports one pool per billing period rather than a five-day window, so the monitor shows it on the long-window row alongside the other providers, leaving the short-window row empty. On-demand spending replaces the pool on that row once any is used, as it already does for Claude Code and Codex.

GitHub Copilot usage comes from `api.github.com/copilot_internal/user`, the endpoint Copilot's own editors and CLI read. The monitor uses the first GitHub login that endpoint accepts, in this order: the `COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, or `GITHUB_TOKEN` environment variable, the Copilot CLI login, then the GitHub CLI login, both read from Windows Credential Manager. Copilot spends one premium-request pool per calendar month, so the monitor shows it on the long-window row and leaves the short-window row empty. Metered chat and completion allowances, as on Copilot Free, are available to themes through `copilot.limits.chat` and `copilot.limits.completions`.

## Updates

The in-app **Check for updates** control checks this fork's latest stable GitHub release. Installing a newer version requires confirmation; the download and replacement helper both verify the GitHub asset's size and SHA-256. Network, metadata, and integrity failures remain errors, never an upstream fallback. There is no UsageHub WinGet listing; recognized WinGet installation paths report that unsupported channel instead of querying the upstream package. See [docs/updater.md](docs/updater.md) for the trust boundary and verification procedure.

## Data and privacy

The monitor reads local sign-in credentials for enabled providers and sends authenticated usage requests directly to their official services. It has no backend service, collects no telemetry, and does not forward credentials or project files to a third-party backend. Registry updates use a shared settings mutex; longer operations use per-account guards. Settings and the account registry live in `%APPDATA%\UsageHub`.

UsageHub does not upload profile files or backup archives; usage polls authenticate directly with the providers. By default, logout, account deletion, and full reset create an encrypted snapshot in `%LOCALAPPDATA%\UsageHub\backups` covering the account's metadata, data files, confirmed owned credential entries, and skill-link metadata: AES-256-GCM with a random data key wrapped by DPAPI `CurrentUser`, current-user ACLs, and no plaintext staging. The snapshot is verified before deletion and retained for seven days; expired snapshots are purged on startup, resume, or the hourly timer. Regenerable-cache cleanup needs no snapshot; immediate purge and explicit irreversible deletion are separate choices. Restoring refuses to overwrite live data and re-checks account-source ownership.

Deletion stays scoped to proven owned data, as described in [Accounts and data safety](#accounts-and-data-safety). UsageHub performs no Windows forensic erasure (event logs, prefetch, crash dumps), no browser-wide wipe, and no system DNS flush, and it touches no other provider's data. Scoped local-data management is an independent implementation; no Claude Cleaner script code was copied.

Credentials are read without modifying the provider files that contain them. When Grok Build rejects a stored token, the monitor asks the Grok CLI to refresh its own session rather than rewriting `auth.json` itself. OpenCode Go credentials saved in a JSON configuration file are plain text and should be protected like a browser session cookie.

None of this is a vendor permission or a "no ban" guarantee: UsageHub uses the same read-only endpoints as each provider's own tools and ordinary user permissions, and providers can change or restrict them.

## Troubleshooting

Hover over the tray icon for the latest failure reason, or check the account
status under **Settings > Providers > Accounts**. Missing credentials, expired
or rejected logins, network failures, HTTP errors, and unexpected usage responses
have distinct messages. Sign in again using the Claude desktop app or the CLI
that owns the affected account, then refresh the monitor.

Standard bar summaries show `!` after a failed refresh or for stale cached data rather than presenting the last percentage as current. Cached authoritative figures remain available for diagnosis; a successful refresh clears the error state. A provider window that was not reported shows `--`, not an invented unused allowance.

Open **Diagnostics** in the dashboard and enable recording to inspect or copy
polling errors. Logging is optional; enable it before reproducing the problem.

Run diagnostics with:

```powershell
usagehub --diagnose
```

The diagnostic log is written to `%TEMP%\usagehub.log`. Application settings are stored in `%APPDATA%\UsageHub\settings.json`.

If the app unexpectedly closes because of a Rust panic, it automatically appends
the panic message, source location, and thread details to the same log, even when
diagnostic recording is disabled. Copy the log before starting a new `--diagnose`
session, which clears it. Review and redact credentials, account identifiers,
and personal information before sharing relevant excerpts in a crash report.
Report suspected vulnerabilities privately using [SECURITY.md](SECURITY.md).

## Accounts and data safety

Account management, encrypted undo snapshots, scoped deletion, shared skills, and taskbar account rows are implemented. The dashboard drives the same `--manage` command surface that scripts use.

### Accounts

Right-click the bar and choose **Open dashboard**, or open **UsageHub** from Start. Under **Settings → Accounts**, choose Claude Code or Codex. The account-name field and **Add account** button are above the account list. Claude opens its isolated official app for sign-in; additional Codex accounts are CLI profiles, not isolated copies of the Codex desktop app. Each compact account card has a deletion/cleanup review action; paths and advanced controls are under **Details**.

The account table labels percentages as **Used**, independently of the bar's Used/Remaining display preference. Reset cells show a countdown; hover for the absolute server UTC time. Source paths and extra reported limits remain in **Account details and advanced actions**, not repeated in every quota row.

- **Add** creates a managed profile under `%LOCALAPPDATA%\UsageHub\profiles\<provider>\<id>`. A default Claude create is a Desktop account: the owned directory is the Desktop user-data dir with its own nested `claude-config`, matching the isolated layout the old Second account already used. Credentials, config, and history belong to that account only.
- **Import** adopts an existing directory in place after an absolute-path and ownership check; rename is display-only and never moves the directory.
- **Open/focus/stop** use the officially registered apps, not a similarly named CLI on PATH. Canonical Default Claude/Codex use native Windows package activation and keep the vendor icon/identity; additional Claude profiles launch in isolated directories. Additional Codex profiles are CLI-only because the desktop app does not offer the required per-profile isolation. Windows open on demand — startup runs the bar only. Custom profiles use their exact marker; canonical External Default excludes sibling profiles and their children. There is no global kill.
- **Per-account CLI** runs the provider CLI with that account's environment (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`-style roots, no token export). The bounded console runner accepts an explicit `--timeout-seconds` (1–3600, default 120) before the CLI `--` separator.
- **Shared skills only**: each account's `skills` entry is wired as a native junction into `%APPDATA%\UsageHub\skills-library` (native mount-point call, never `mklink`). That library is the sole shared state; links are backed up as metadata and removed by inode, never traversing the library.
- **Widget accounts** update automatically in the built-in classic themes and the imported `left-dock-usage` classic layout. Enabled, pinned profiles occupy provider columns; additional profiles use the overflow chooser when space is limited. Other custom themes remain unchanged unless they opt in with an account row.

### Deletion and backups

- Destructive work starts from the read-only `cleanup preview` (every target with ownership, size, and backup coverage) and requires `cleanup apply --confirm <account-id>` in the CLI or a typed confirmation in the dashboard.
- In the dashboard, deletion opens a separate review with the selected account, read-only preview, and typed stable-id confirmation. Cancel changes nothing. The default is an encrypted seven-day undo snapshot; deleting without a backup is a separate explicit irreversible choice. Canonical External Default is presented as cleanup, not as an ordinary removable managed profile.
- To clean the main account, use **Cleanup...** on its Default card. For external canonical data choose **Full reset** and explicitly enable **Include external Default data**; without that opt-in the preview/apply path stays blocked. **Preview** only reviews targets. Changing account or scope invalidates the old review and confirmation; nothing is removed until the matching Apply confirmation.
- `cache`, `logout`, `delete`, and `reset` scopes stay distinct. Cache covers only proven regenerable browser caches. Logout removes auth material (credential file, confirmed owned Credential Manager targets, session state). Delete adds account config and history. Reset covers every known user-owned source for the selected account; external Default canonical data requires explicitly selecting that account (a dashboard opt-in, or its id plus matching `--confirm` in the CLI).
- Logout, account deletion, and full reset verify the default encrypted snapshot before removal (see [Data and privacy](#data-and-privacy)); pure cache cleanup needs none. `--no-backup` is the explicit irreversible path.
- Standalone `backup create` captures profile metadata, confirmed owned credentials, Desktop `config.json`/`Local State`, the separate CLI home, and launcher metadata. Automatic destructive-cleanup snapshots additionally cover every deletion target, including the complete owned Desktop directory.
- Restore is all-or-nothing: any live conflict, mismatched account id, or changed source aborts before writing. Expired server sessions may require signing in again.
- Snapshot expiry is checked at startup, on resume, and hourly; expired snapshots are purged. Purging removes the ciphertext, the wrapped key, and the tombstone together.

### Migrating from the old monitor

`usagehub --manage migration import` binds an unmapped canonical Default to its actual Desktop token cache, adopts Second in place, copies themes, and imports allowlisted non-auth preferences only into a fresh fork. Existing custom mappings and later fork edits are preserved. `usagehub --manage migration cutover --confirm cutover` disables the old sync task before touching plaintext exports and writes a snapshot with a restore-based rollback script. The exact layout, confirmation rules, and old `.claude_backup_*` handling are documented in [docs/migration.md](docs/migration.md).

## Build from source

Install [Rust via rustup](https://www.rust-lang.org/tools/install), then run:

```powershell
cargo build --release
```

The repository pins Rust 1.95.0 in `rust-toolchain.toml`; rustup automatically
selects this version. `.cargo/config.toml` builds the `x86_64-pc-windows-gnullvm`
target, so the pinned host toolchain and the target are identical. When upgrading
Rust, update `rust-toolchain.toml`, `.github/workflows/release.yml`, and the
`rust-version` in `Cargo.toml` together.

The executable will be created at `target\x86_64-pc-windows-gnullvm\release\usagehub.exe`, linked with the
self-contained llvm-mingw toolchain documented in
[docs/build-windows.md](docs/build-windows.md) (toolchain, linker, and `PATH`
setup, including the `llvm-windres` requirement). The build links the C runtime
statically, so the executable does not require a separate Microsoft Visual C++
Redistributable installation; it still uses built-in Windows system libraries.
The fork's tag-triggered GitHub release workflow uses an explicit Rust 1.95.0
MSVC host/target on `windows-latest` and publishes `usagehub.exe`. Local gnullvm
builds use the separate setup above; no upstream WinGet package is published.

See [dependency security](docs/dependency-security.md) for automated dependency
updates, CI security checks, and the commands to run those checks locally.

## Contributing and community

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, reporting bugs,
and submitting pull requests, and follow our [Code of Conduct](CODE_OF_CONDUCT.md).
For suspected vulnerabilities or credential exposure, use the private reporting
instructions in our [security policy](SECURITY.md).

## License

Licensed under the [MIT License](LICENSE). UsageHub is a fork of Claude Code Usage Monitor (Copyright (C) 2026 Code Zeno Pty Ltd); third-party attributions, including the Leonxlnx multi-account work the account manager adapts, are recorded in [LICENSE](LICENSE). Scoped local-data management is an independent implementation; no Claude Cleaner script code is copied.
