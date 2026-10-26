# Historical manager implementation contract

This document records the initial parallel-work design, not the current
implementation status or an outstanding-work checklist. The manager, CLI,
migration and native dashboard are implemented and locally deployed.
Start with [HANDOFF.md](HANDOFF.md) for current state and operational guidance.
Rust signatures and fixture locations below are historical sketches: consult
the actual source before editing; do not recreate modules or reapply integration
patches because an old paragraph says a slice has not landed.

## 0. User overrides (highest priority)

1. Exact limits AND resets come from the original usageBar server poller
   ONLY. In any authoritative usage path there is MUST-NOT:
   switcher estimates, plan-usage-history inference, weekly corrections, or
   manual reset overrides.
2. Detailed reset UI shows actual absolute server reset dates as well as the
   countdown, plus provenance/time; shows `unknown` when the server sent no
   reset. Estimates may appear only in explicitly labeled non-authoritative
   spots (e.g. passive stale-cache badges), never in the authoritative path.
3. `--manage` CLI MUST support noninteractive JSON output/errors usable
   through redirected stdout/stderr under the `windows_subsystem` build, plus
   a read-only poll/snapshot command for the actual bar GET scenario and
   smoke fixtures.
4. The runtime source MUST distinguish `desktop_dir` directly
   (`SourceKind::DesktopDir`); a DesktopDir profile reads config.json + Local
   State in that directory and never falls back to CLI credential discovery.

## 1. Source of truth and operation serialization

- `SettingsFile.accounts` is the ONLY registry. No divergent files, no
  in-memory registries that outlive one operation.
- Every manager mutation uses `manager::state`:
  `SettingsOperationGuard::acquire` (`Local\UsageHubSettingsGuard`,
  session-scoped = current-user serialization, 5s default timeout) →
  `load_settings()` fresh → mutate → `save_settings()` (normalizes + atomic
  `MoveFileExW` replace). Helpers: `mutate_settings`,
  `mutate_settings_timeout`, `load_fresh`.
- Background UI operations run on a worker thread (UI stays responsive) and
  the UI calls `load_fresh()` when the worker finishes. Window/studio code
  MUST NOT hold the guard across UI waits.
- Retired IDs: `ProviderAccounts.used_ids` is append-only; IDs are never
  reused so theme bindings never retarget a new account. Rename = display
  fields only (`rename_display` / `ProviderAccounts::rename`); `id` is
  immutable after allocation (normalize only replaces invalid/duplicate ids).

## 2. Data roots (fork namespace `UsageHub`; never the original app dirs)

| Purpose | Path |
|---|---|
| Config (settings, themes, shared skills) | `%APPDATA%/UsageHub` = `paths::fork_config_dir` (`USAGEHUB_CONFIG_ROOT` override, absolute only) |
| Secrets, backups (non-roaming) | `%LOCALAPPDATA%/UsageHub` = `paths::fork_local_dir` (`USAGEHUB_LOCAL_ROOT` override, absolute only) |
| Encrypted deletion-undo snapshots | `%LOCALAPPDATA%/UsageHub/backups/<provider>/<account_id>` = `paths::backup_dir_for` |
| New managed profiles (initial default) | `%LOCALAPPDATA%/UsageHub/profiles/<provider>/<account_id>` = `paths::managed_account_dir` |
| Shared skills library | `%APPDATA%/UsageHub/skills-library` = `skills::shared_skills_dir` |
| Profiles-root override (tests/portable) | `%USAGEHUB_PROFILES_ROOT%` = `paths::profiles_root` (absolute only, wins over the local-root default) |

Root rules: every override must be absolute; `paths::validate_root_overrides()` fails fast (CLI exit 2 once `manager::cli` lands) instead of surfacing misleading `join_within` "escapes its root" errors. The smoke binary sets all three roots under `target/smoke/roots` for full isolation. `app_settings::app_data_directory()` == `paths::fork_config_dir()` (single root definition; `%APPDATA%` is only the known-folder fallback). In `cfg(test)`, `fork_config_dir()` is the per-test-thread `app_settings::app_data_directory()` fixture and `fork_local_dir()` is that fixture plus a `local` suffix; env overrides are ignored so parallel tests never race on process-global state or touch real `%APPDATA%`/`%LOCALAPPDATA%` dirs.

Adopted legacy paths (Second) stay in place: validated with
`require_absolute` only, never moved on rename. While in development (before
the migration smoke pass), existing `D:/Soft` apps and running user profiles
MUST NOT be edited or stopped. After smoke, final approved migration/account
operations on them are in scope and follow the migration slice's preview +
rollback policy — this protection is a development-phase guard, not a global
ban.

Approved roots: backups default to `%LOCALAPPDATA%` (non-roaming, DPAPI
CurrentUser, current-user ACL) and the shared skills library defaults to
`%APPDATA%` (allowed: the library never stores auth material — no tokens,
credential files, or credential targets).

## 3. `AccountProfile` contract (`src/accounts.rs`)

Serde-default metadata (all backward compatible with existing settings.json):

- `desktop_dir: String` — explicit Desktop user-data-dir. Empty = unconfigured.
- `ownership: AccountOwnership` — `Managed` (created/adopted) vs `External`
  (default; read-only unless an explicit full-reset flow opts in).
- `pinned: bool`, `sort_order: i32`, `color: String`, `label: String` —
  display only; `ProviderAccounts::ordered` sorts pinned → sort_order → name.
- `credential_targets: Vec<String>` — confirmed owned Windows Credential
  Manager targets. Only listed targets may be backed up/deleted; unknown
  targets are reported, never guessed; no broad prefix deletion.
- `id` immutable; `used_ids` never reused; rename touches `name` only.
- Legacy `credential_path(provider)` semantics preserved for CLI/Codex
  callers. New explicit access: `desktop_dir_resolved`,
  `config_dir_resolved`, `source_kind`, `is_managed`, `same_source`
  (now includes `desktop_dir`).

`SourceKind::{CredentialFile, DesktopDir, ProviderDefault}` — the poller MUST
branch DesktopDir to the DesktopProfile reader (config.json + Local State +
owned Credential Manager via the Rust desktop token reader; no plaintext
exports, no silent `claude -p` token refresh, read-only usage GET).
`desktop_dir` is Claude-only: any non-Claude profile that sets it is rejected
(`AccountContext::from_profile` errors with the profile id). `default_credential_path`
covers Claude/Codex only (`None` otherwise); `"default"` ids compare
case-insensitively so legacy `"Default"` profiles keep provider-default discovery.

## 4. Core shared types (`src/manager/mod.rs`)

- `ManagerResult<T> = Result<T, String>`.
- `AccountContext::{provider, profile, source, config_dir, desktop_dir,
  credential_file, ownership}` via `from_profile` (explicit paths only;
  discovery fallbacks stay in the poller; DesktopDir and ProviderDefault yield
  no `credential_file`; CredentialFile requires an explicit file/dir and
  rejects a source-less non-default profile with its id).
- `BackupInfo::{backup_id, provider, account_id, created_unix, expires_unix,
  path, size_bytes, manifest_hash, credential_targets}` +
  `BACKUP_RETENTION_SECS` (7d), `expiry_for`, `is_expired`.
- `CleanupScope::{AccountCache, AccountLogout, AccountLocalData, FullReset}`:
  `AccountLogout` signs out (credential files, confirmed owned credential
  targets, session/token state; config/history/skills kept) and is distinct
  from `AccountLocalData` (logout plus config/history deletion).
- `CleanupTargetKind::{Cache, Auth, Config, History, SkillsLink,
  CredentialTarget, Directory}`,
  `CleanupTarget::{path, kind, owned, size_bytes, covered_by_backup}`,
  `CleanupPlan::{scope, provider, account_id, targets, backup_id, warnings}`.
  Apply enforces ownership and verified backup coverage directly; pure cache
  cleanup and explicit irreversible deletion have the documented exemptions.
- Link ownership rule: deletion owns the link inode, never the target. A
  `SkillsLink` proven to be an account link (`validate_skill_link` against
  the shared-library root) is `owned = true` and removed with
  `remove_link_only` (classifies the inode with `symlink_metadata` +
  `FILE_ATTRIBUTE_DIRECTORY` and a dual-try fallback, never following a
  dangling target). Preserved links are
  excluded from the plan entirely. Unknown credential targets and unvalidated
  links are never owned.

## 5. Original API sketches (source code is authoritative)

### `manager::lifecycle`

```rust
fn create_account(provider: ProviderId, name: &str, opts: NewAccountOpts) -> ManagerResult<AccountProfile>;
fn import_account(provider: ProviderId, path: &Path, opts: ImportOpts) -> ManagerResult<AccountProfile>;
fn rename_account(provider: ProviderId, id: &str, new_name: &str) -> ManagerResult<()>;
fn open_account(provider: ProviderId, id: &str) -> ManagerResult<()>;
fn focus_account_window(provider: ProviderId, id: &str) -> ManagerResult<bool>;
fn stop_account_processes(provider: ProviderId, id: &str) -> ManagerResult<StopReport>;
fn account_status(provider: ProviderId, id: &str) -> ManagerResult<AccountStatus>;
fn account_cli_env(ctx: &AccountContext) -> ManagerResult<Vec<(String, String)>>;
```

`NewAccountOpts{desktop_dir: Option<PathBuf>, config_dir: Option<PathBuf>,
ownership: AccountOwnership, pinned: bool, ...}` — new dirs under
`managed_account_dir`; never invent ids (use `ProviderAccounts` allocate via
`mutate_settings`). `open` launches the installed official app as a separate
account window (no reparenting/embedding, no redistribution). `stop` quiesces
the selected account only (graceful close, verify locks released; no global
kill). `status` surfaces read-only poller data; `cli_env` maps a context to
`CLAUDE_CONFIG_DIR`/`CODEX_HOME`-style env (no token export).

### `manager::backup`

```rust
fn create_backup(ctx: &AccountContext) -> ManagerResult<BackupInfo>;
fn list_backups(provider: ProviderId, account_id: &str) -> ManagerResult<Vec<BackupInfo>>;
fn verify_backup(backup_id: &str) -> ManagerResult<BackupInfo>;
fn restore_backup(backup_id: &str) -> ManagerResult<()>;
fn purge_expired(now_unix: u64) -> ManagerResult<Vec<BackupInfo>>;
fn purge_backup(backup_id: &str) -> ManagerResult<()>;
```

Streaming AEAD backup to `%LOCALAPPDATA%`, random data key wrapped with
DPAPI CurrentUser, current-user ACL on files. Manifest + restore verification
BEFORE live-data deletion; restore to same stable paths/ids (conflict =
refuse; expired server sessions may need re-login). Back up confirmed owned
credential entries via supported Windows APIs only. Expiry checked on
controller timer, startup, resume (purge runs next start if stopped). UI also
offers immediate purge and an explicit irreversible deletion option; the
default is the 7-day undo, not zero-trace deletion. Purge removes personal
tombstone metadata with the backup.

### `manager::cleanup`

```rust
fn preview_cleanup(ctx: &AccountContext, scope: CleanupScope) -> ManagerResult<CleanupPlan>;
fn apply_cleanup(plan: &CleanupPlan) -> ManagerResult<CleanupReport>;
```

`preview` enumerates exact targets with proven ownership and verified-backup
coverage. Proven account skill links appear as `SkillsLink` with
`owned = true` (link inode only) and are removed with
`skills::remove_link_only`, which never traverses the target; preserved
links are excluded from the plan, never listed unowned. `apply` checks proven
ownership and verified snapshot coverage directly (except pure cache cleanup
or explicitly confirmed irreversible deletion). Logout removes auth material only
(credential files, confirmed owned credential targets, session state);
config/history/skills are kept. Cache scope = proven regenerable browser
caches only (never LocalStorage/IndexedDB/ServiceWorker/user history).
FullReset covers every known user-owned Claude data source only with
explicit selection; external Default data opt-in only. No Windows forensic
erasure, no browser-wide wipe, no system DNS flush, no Codex/provider
collateral. Canonical comparison: `lowered` normalizes both slash styles
and `\\?\` / `\\?\UNC\` / `\??\` / `\??\UNC\` prefixes before case comparison,
so imported `C:/` paths and junction `read_link` targets match. Deletion path
guard: `paths::first_reparse_point` before any destructive work; stop on any
unexpected reparse point.

### `manager::cli` (`--manage`)

Placement (once this module lands; see §8): parsed BEFORE window startup and
the single-instance guard; read-only commands skip the instance mutex but
still honor the settings-operation mutex for any settings touch.

- Never `AllocConsole`; write JSON to stdout / errors to stderr so
  `usagehub.exe --manage ... > out.json` works under `windows_subsystem`.
- Envelope: `{"ok":true,"data":{...}}` / `{"ok":false,"error":{"code":"...","message":"..."}}`.
- Exit codes: `0` ok, `2` usage error, `3` operation error.
- Commands: `status [--json]`, `list [--provider claude|codex]`,
  `poll --provider <p> [--account <id>] [--account all]`,
  `snapshot --provider <p> [--account <id>]`,
  `backup create|list|verify|restore|purge ...`,
  `cleanup preview|apply ...`, `doctor`.
- `poll`/`snapshot` are READ-ONLY and MUST call the original usageBar server
  poller path (`poller::poll_accounts` semantics: one job per distinct
  source, shared-source grouping, normal rate-limit handling) — no settings
  writes, no usage-cache writes, no token refresh prompts. Output schema
  mirrors `AppUsageData` accounts entries with per-account
  `{provider, account_id, source, limits, resets, provenance, error}` where
  resets carry absolute server `resets_at` + provenance/time or `unknown`.
- Smoke fixtures for tests: `tests/fixtures/manage/poll-<provider>.json`
  (canned server GET payloads) and `snapshot-<provider>.json` (expected CLI
  output); the CLI slice owns them.

### `manager::migration`

- Register Second in place (`ownership=Managed`, immutable old path).
- MUST disable/stop the legacy `usageBar Claude accounts sync` scheduled
  task BEFORE cleaning plaintext token exports; defer irreversible cutover
  until new reader + UI + backup/restore smoke pass.
- Remove old duplicate binaries/autostarts/launchers only in final cutover,
  with preview and rollback policy. Old plaintext backups: explicit
  encrypt/purge/leave choice. Never delete old profiles just because the old
  program is retired. Preserve uncommitted `ClaudeAccounts` sources.

### Studio UI integration

- Account list repeats an account row template (or native row component) per
  `ordered()` profile; `accounts.count` alone MUST NOT drive UI. Width
  budget + pins + overflow popup (no Start/icon coverage); error state never
  removes the row.
- Simple add/open/focus/rename; destructive actions behind preview; manual
  entry stays an `input` + datalist (new values allowed).
- Reset display rule (authoritative): absolute server date + countdown +
  provenance/time; `unknown` when absent. No estimates in this path.
- Ukrainian localization for new strings; port lifecycle/shortcuts/AUMID/
  CLI/usage-fallback/diagnostics patterns; no unrelated cleaner scanners or
  account auto-rotation.

## 6. Historical first-slice checkpoint (superseded)

- `src/accounts.rs`: extended `AccountProfile` + `AccountOwnership` +
  `SourceKind`, `same_source`/`source_kind`/`credential_path` (legacy
  preserved)/`desktop_dir_resolved`/`config_dir_resolved`/`rename_display`/
  `is_managed`, `ProviderAccounts::{find, find_mut, ordered, rename}`.
- `src/manager/{mod,paths,state,skills}.rs`: data roots, reparse/link
  guards, serialized settings access, shared-skills helpers, core
  `AccountContext`/`BackupInfo`/`Cleanup*` types.
- `src/main.rs`: `mod manager;` registered.
- Untouched in this slice: all other widget providers, ThemeStudio, native
  stack, existing `D:/Soft` apps and running user profiles (development-phase
  guard per §2; final approved operations after smoke stay in scope).

## 7. Historical risks and open questions

- New fields ride `#[serde(default)]`: old settings.json loads cleanly, but
  downgrading after new fields are set drops them (acceptable; settings are
  forward-only across the fork cutover).
- `is_within` is lexical + case-insensitive over slash- and
  `\\?\`-normalized spellings; symlink-equivalent paths that differ
  textually are caught at use time by `first_reparse_point`, not by the
  lexical check — destructive slices must call both.
- `Local\` mutexes are session-scoped: per-user RDP sessions stay
  independent, which matches the single-instance guard semantics.
- Skills-link creation (junction vs symlink privilege) is the lifecycle
  slice's problem; this contract only constrains validation/removal.

## 8. Historical `--manage` integration sketch (already integrated)

In `main()`, after the `--diagnose` block and BEFORE
`studio_app::handle_cli_mode` / `window::run`:

```rust
if let Some(exit_code) = manager::cli::handle_manage_mode(&args) {
    if diagnose_enabled {
        diagnose::log(format!("manage cli exited with code {exit_code}"));
    }
    std::process::exit(exit_code);
}
```

The actual entry point is implemented in `src/main.rs` and
`src/manager/cli.rs`. This snippet is retained as design history, not a patch
to apply again. Use the current CLI help and source for supported arguments.
