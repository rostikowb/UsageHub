# UsageHub

**Your AI usage limits, at a glance in the Windows taskbar.**

[![Windows](https://img.shields.io/badge/platform-Windows-blue)](#requirements)
[![License: MIT](https://img.shields.io/badge/license-MIT-yellow)](LICENSE)

UsageHub is a Windows taskbar monitor for usage and reset times across Claude Code, Codex, and other AI tools. Keep the bar in view; open the dashboard only when you need settings or account management.

**[Download the latest UsageHub release](https://github.com/rostikowb/UsageHub/releases/latest)** · [Quick start](#usage) · [Accounts and cleanup](#accounts-and-data-safety) · [User guide](USER_GUIDE.md)

![Usage monitor in the Windows taskbar](.github/animation.gif)

## Requirements

Windows 10 or 11 and a supported provider with a signed-in account. Provider-specific setup is below.

## Installation

Download `usagehub.exe` from the [latest release](https://github.com/rostikowb/UsageHub/releases/latest) and place it in a user-writable folder. UsageHub can run alongside the upstream monitor with separate settings and application identity. For source builds, see [Windows build setup](docs/build-windows.md).

## Usage

In PowerShell, from the folder containing the executable:

```powershell
.\usagehub.exe
```

Open the settings dashboard from the tray menu, or run:

```powershell
.\usagehub.exe --dashboard
```

Sign in through your provider's own app or CLI, then enable the provider under **Settings → Providers**. Choose used or remaining allowance under **Settings → Display → Usage direction**. See the [user guide](USER_GUIDE.md) for display options and Theme Studio; theme authors can use `.display` bindings such as `{claude.session.display:usage_line}` to follow the selected direction.

## What it does

- Shows provider-reported usage and reset times in a customizable taskbar bar, with themes, tray controls, and optional Windows startup.
- Manages Claude Code and Codex accounts, including pinned account rows and an overflow chooser.
- Keeps the bar resident and opens the separate GPU-accelerated dashboard on demand. Hardware Direct3D 11 is preferred; software WARP and OpenGL initialization fallbacks are available. See [dashboard rendering](docs/dashboard-rendering.md).
- Uses provider usage responses, not model prompts, CLI probes, inferred allowances, or manually entered reset dates. Missing values remain unknown; failed or stale refreshes are marked rather than presented as current.

## Provider setup

| Provider | Sign-in or setup |
| --- | --- |
| Claude Code | Sign in with the Claude Code CLI or Claude desktop app. Windows and WSL stored logins are supported. |
| Codex | Install and sign in to the Codex CLI. |
| Google Antigravity | Sign in to Antigravity. |
| OpenCode Go | Configure a workspace ID and session cookie as described below. |
| Cursor | Sign in to Cursor; the local session is detected automatically. |
| Grok Build | Sign in through the Grok CLI; an API key alone is not enough. |
| GitHub Copilot | Sign in with the Copilot CLI (`copilot`, then `/login`) or GitHub CLI (`gh auth login`). |

Enable the providers you want under **Settings → Providers**. For OpenCode Go, set `OPENCODE_GO_WORKSPACE_ID` and `OPENCODE_GO_AUTH_COOKIE`, or put `workspaceId` and `authCookie` in `%APPDATA%\opencode-go\config.json`. The workspace ID is in `https://opencode.ai/console/<workspaceId>/go`; the cookie is `__Host-console_session` from your signed-in `opencode.ai` session, including its name. Treat it as a password. `OPENCODE_GO_CONFIG_FILE` can select another config path.

## Accounts and data safety

Open **Settings → Accounts**, choose Claude Code or Codex, enter a name and select **Add account**. Sign in using the official app or CLI. Managed Claude Desktop accounts have isolated user-data directories; additional Codex profiles are **CLI-only**, not isolated Codex Desktop copies. Only the skills library is deliberately shared among managed accounts; credentials, config, and history are not shared through it.

To clean the main account, select **Cleanup...** on its **Default** card. Review the preview and type the account ID before applying. For external Default data, choose **Full reset** and explicitly enable **Include external Default data**; without that opt-in it remains blocked. Preview alone removes nothing.

Destructive logout, deletion, and full reset normally create an encrypted seven-day undo snapshot; deleting without a backup is a separate explicit irreversible choice. Regenerable cache cleanup does not require a backup. Cleanup is limited to proven owned data: it is not forensic erasure, a browser-wide wipe, or removal of vendor telemetry. See [migration and recovery](docs/migration.md) for account adoption, shared skills, and rollback.

## Data and privacy

UsageHub reads local credentials for enabled providers and sends authenticated usage requests directly to their services. It has no hosted backend or analytics of its own. Settings are stored in `%APPDATA%\UsageHub`; encrypted undo snapshots are stored in `%LOCALAPPDATA%\UsageHub\backups`. Provider access can change, and use of their endpoints is not a vendor permission or guarantee against account restrictions.

OpenCode Go cookies stored in a JSON config are plain text. Protect them and redact credentials and personal data before sharing logs. Report suspected vulnerabilities privately using [SECURITY.md](SECURITY.md).

## Updates

The portable in-app updater checks this fork's latest stable release and requires confirmation before installation. It verifies the `usagehub.exe` asset's size and SHA-256 against GitHub release metadata; this is not a publisher signature. UsageHub has no WinGet package. See [updater verification](docs/updater.md) for its trust boundary.

## Troubleshooting

Check the tray icon or account status for refresh errors, then sign in again through the affected provider's app or CLI. An unreported quota displays `--`; stale or failed refreshes show `!`. Dashboard **Diagnostics** can record polling errors. Review and redact logs before sharing them.

## Build from source

See [Windows build setup](docs/build-windows.md) for the pinned Rust and LLVM toolchain, prerequisites, and commands. The [changelog](CHANGELOG.md) records version history.

## Maintainer handoff

For project context and operational details, see [docs/HANDOFF.md](docs/HANDOFF.md), [migration and rollback](docs/migration.md), and [dashboard rendering](docs/dashboard-rendering.md).

## Contributing and community

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md). For security issues, use the private reporting instructions in [SECURITY.md](SECURITY.md).

## License

[MIT License](LICENSE). UsageHub is a fork of [Claude Code Usage Monitor](https://github.com/CodeZeno/Claude-Code-Usage-Monitor), Copyright (C) 2026 Code Zeno Pty Ltd. The account manager also adapts multi-account work by Leonxlnx; attribution is preserved in [LICENSE](LICENSE). This fork is published at [rostikowb/UsageHub](https://github.com/rostikowb/UsageHub), not the upstream repository.
