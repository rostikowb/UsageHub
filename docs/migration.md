# Migration: legacy usageBar → UsageHub

Side-by-side cutover from the old monitor to the UsageHub fork namespace.
All commands run through the fixed JSON surface:

```
usagehub.exe --manage migration preview|import|cutover [--confirm cutover]
usagehub.exe --manage migration legacy-backup --path PATH --action encrypt|purge|leave --confirm NAME
```

Stdout carries success JSON (`{"ok":true,…}`); the error envelope
(`{"ok":false,…}`) goes to stderr only; exit codes are `0`/`2`/`3`. The
dashboard invokes the same commands through a captured backend, so in a shell
redirect stdout/stderr yourself (`> out.json 2> err.txt`).

## Observed legacy layout (production machine, parent handoff)

The old monitor's settings live under `%APPDATA%\ClaudeCodeUsageMonitor`;
`D:\Soft\usageBar` holds only the retired binary and its Python sync script,
with no profile exports. The configuration root is therefore imported
directly: it is the first known-namespace candidate and the only content-
bearing one, while the install dir is binary-only and is never used as a
settings/theme source.

| Artefact | Value |
|---|---|
| Old monitor binary | `D:\Soft\usageBar\claude-code-usage-monitor.exe` |
| Scheduled task (exact) | `usageBar Claude accounts sync` — Ready, runs `D:\Soft\python.exe D:\Soft\usageBar\sync-claude-accounts.py` (unquoted original args) |
| HKCU Run value (exact) | `ClaudeCodeUsageMonitor` = `"D:\Soft\usageBar\claude-code-usage-monitor.exe" --diagnose` |
| Legacy config dir (actual) | `%APPDATA%\ClaudeCodeUsageMonitor` — the imported `settings.json`/`themes` root plus the plaintext `profiles/second/.credentials.json` bridge export |
| Old tool install dir | `D:\Soft\usageBar` — the EXE and `sync-claude-accounts.py` only; no profiles, no settings, no themes (binary-only, never a source) |
| Official Claude autostart in Run | none |

## Shared skills (approved: shared, never per-account copies)

Known skills homes are derived from the registry: the Default profile and any
Second profile (exact id or name), each with BOTH its `desktop_dir/skills`
and `config_dir/skills` (the CLI reads the nested one). An External Default
with no explicit mapping — or no registry Default at all — falls back to the
canonical provider-default home (`$CLAUDE_CONFIG_DIR/skills`, else
`<home>/.claude/skills`); that fallback is treated as managed, so an existing
real `.claude/skills` is imported, backed up, and junctioned. The canonical
Default bound by `import` to the real Desktop dir keeps the same approved
`.claude/skills` target: its separate canonical CLI home stays junction-
eligible while the external Desktop tree itself, and any other explicit
External base, stays report-only (never created, replaced, or junctioned). A
missing junction-eligible home is simply created as a junction.

- `preview` lists every home (`exists`, `is_link`, `link_valid`, entry
  count), each home's conflict names and `preview_error`, and the totals in
  `skills.skills_conflicts`. It is strictly read-only and never creates the
  shared library.
- `import` copies every existing real home into the shared library with
  `skills::import_skills_from(home, false)`: new entries imported, identical
  ones skipped, conflicts reported but never overwritten. Per-home failures
  are recorded in `skills_import` without failing the adoption.
- `cutover --confirm cutover` junctions homes only after all three hold per
  real directory: a conflict-free import, a verified encrypted backup
  (`import_legacy_backup` + `verify_backup`, account `default`/`second`),
  and junction-eligible ownership (Managed, or the canonical CLI home of the
  explicitly canonical External Default; anything else is report-only).
  `create` junctions a missing junction-eligible home; `replace` moves the
  real dir into a unique stable `snapshot/skills/<kind>_<desktop|config>/`
  slot first and junctions only afterwards, rolling the dir back if
  junctioning fails. Refused homes (conflicts, external, quoted paths,
  backup failures) are listed, never created or silently replaced, and
  report-only homes never make cutover non-`noop` by themselves.
- Rollback: `rollback.cmd` runs `rmdir` on each junction (link inode only,
  never the library) and moves staged originals back; `manifest.json`
  records per-home file/byte counts, backup ids, and staged paths.

The top-level `migration preview` also corroborates the task (`Task To Run`
cell) and reports any additional hint-matching tasks **report-only** —
cutover touches only the exact task above.

## Command reference

### `migration preview` (read-only)

Scans: exact task presence + run command, hint-matching task names,
`HKCU\…\Run` values referencing the legacy tool, legacy-named `.lnk`
launchers in the Startup folder, legacy settings dirs (known-namespace
`%APPDATA%`/`%LOCALAPPDATA%` candidates named `ClaudeCodeUsageMonitor`,
`ClaudeCodeUsage`, `claude-code-usage`, `ClaudeAccounts`, `usageBar` or
`UsageBar`, content-bearing config roots ranked first; the binary-only
`D:\Soft\usageBar` install dir is appended last so it can never shadow the
real config root), the current registry,
exact recognized Second dirs (with desktop-marker and bridge flags), and the
exact owned plaintext exports: the bridge copy
`%APPDATA%/ClaudeCodeUsageMonitor/profiles/second/.credentials.json` plus
credential-shaped (`.credentials.json` / `credentials*.json`) Claude Second
(`second/`-scoped) files confirmed by the old `settings.json` or the sync
script that exist as files under a confirmed legacy root — never themes,
assets, configs, settings, or Codex paths, and never filename-hint sweeps.
The old monitor's actual settings document
`%APPDATA%\ClaudeCodeUsageMonitor\settings.json` IS the confirmed legacy root:
it is a content-bearing known-namespace candidate, so `preview` reports its
importable preference keys (`legacy_settings`) and `import` reads it directly
for the one-time preference import; theme/asset import uses the same confirmed
root. The `D:\Soft\usageBar` install dir is binary-only (exe + sync script)
and is never a settings/theme source. `default_binding` additionally reports
whether the canonical Claude Default would be bound to the real Desktop dir
(`would_bind`/`already_mapped`/`missing_desktop`/`refused`/`skipped`) without
writing anything. Fresh users with no old dirs get empty findings and
`ok:true` — never an error.

### `migration import` (bind Default + adopt Second + themes)

1. Binds the canonical Claude Default to the real Desktop data dir when the
   built-in Default is otherwise unmapped (all source fields blank) and
   External: `desktop_dir` = the physical Desktop user-data dir (a
   `%APPDATA%\Claude` junction into `%LOCALAPPDATA%\Claude-Data\claude_*` is
   resolved, so no link is stored) and `config_dir` = the separate canonical
   CLI home (`$CLAUDE_CONFIG_DIR`, else `<home>\.claude`). Only path fields
   are written — id, name, preferences, display metadata and any customized
   explicit mapping are preserved, ownership stays External (its data is
   covered only by an explicit FullReset), and the DesktopDir source stops the
   poller from falling back to `~/.claude/.credentials.json`, which is a
   different CLI login. Requires `config.json` + `Local State`; with no
   canonical Desktop dir carrying those the Default is left as-is and
   `default_binding` reports `missing_desktop` plus the searched dirs — a CLI
   login is never substituted. A Default whose account guard is held reports
   `busy` and is never forced. The step runs before and independently of the
   Second plan, so a Desktop-only machine is covered too.
2. Copies `themes/*.json` and `assets/*` from the confirmed legacy dir into
   the fork namespace, skipping files that already exist (never overwrites).
3. Adopts the exact recognized Second **in place** (never a substring match,
   never the plaintext bridge as a Desktop): the known observed root
   `D:/Soft/claude-multi-account/accounts/Second` or an exact legacy
   `<root>/profiles/second` dir, and only with Desktop layout markers
   (`config.json` + Local State) plus a passing prospective ownership check
   through `lifecycle::owned_deletion_root` (the same gate cleanup enforces).
   Registered with stable id `second`, `desktop_dir` = the Second dir itself
   and `config_dir` = its existing `claude-config` child (when present,
   matching lifecycle's canonical nesting). A recognized source must be a
   directory with the Desktop markers; a bare `.credentials.json` bridge is
   never adopted as a Desktop and must go through an explicit
   `--manage import`; anything else is refused with guidance. Fails when no
   Second source exists.
4. Imports the allowlisted legacy non-auth preferences ONLY into a fresh fork
   (one with no settings file of its own yet), exactly once: poll interval,
   language, countdown/lock, dashboard size/opacity, placement
   override/offset/index, widget visibility, and provider visibility
   switches. Repeat runs preserve the fork's own edits. The legacy account
   registry, `selected`/`used_ids`, `credentials_path`, credentials, usage
   cache, and update stamp are never imported.
5. First-migration pins: when a provider registry has no actual user pin yet,
   the enabled Default and Second accounts are pinned (nothing else);
   pre-existing pins and all display metadata stay untouched.
6. Rewrites `active_theme_path` only if it still points into a legacy dir
   **and** the same file now exists in the fork themes dir.
7. Copies existing skills homes into the shared library non-destructively
   (see Shared skills above).
8. Never reads credential files; the only Default mutation is the path
   mapping from step 1 (ownership and data stay External).

### `migration cutover --confirm cutover` (parent runs after smoke)

Strict order — no plaintext credential ever sits in the snapshot:

1. Snapshot dir `%LOCALAPPDATA%/UsageHub/migration/snapshots/<unix>/`:
   `rollback-pending.json` first, then task XML. The manifest and generated
   `rollback.cmd` + `rollback.reg` are written after the encrypted backups and
   before any destructive step. Metadata only; no secrets.
2. Disable + stop the exact legacy task FIRST (`/End` tolerated when idle,
   `/DISABLE` required, Disabled state re-queried). Abort when the task
   cannot be stopped — nothing deleted yet, state is reversible.
3. ONE encrypted targeted archive PER export (`backup::import_legacy_paths`,
   account `legacy_usagebar`), each with its own confirmed root and verified
   (`verify_backup`). Merge-missing restore recreates an absent export without
   touching still-occupied settings/themes — rollback actually succeeds
   instead of refusing on conflict. Never merged into an active account.
   Any abort from here re-enables the task (nothing destructive has run yet)
   and returns the snapshot/pending-metadata path.
4. Manifest + restore-based rollback script: per-export conditional restores
   using the absolute path of the UsageHub executable that ran cutover
   (`if not exist <path> "<installed exe>" --manage backup restore --backup ID --confirm ID`,
   so present files are never overwritten). Failed recovery stops rollback
   before task or launcher restoration; skills restores use staged moves with
   per-home file/byte metadata.
5. Remove legacy Run values; **move** launchers into the snapshot;
   **DELETE** plaintext exports (only after their verified encrypted archive —
   never moved or copied as plaintext).
6. Live profile directories and old repo checkouts are never touched.
7. Empty findings → `{"cutover":"noop"}` without creating anything.

Rollback: first stop UsageHub and disable its own Run entry to avoid running
both monitors. Run `rollback.cmd` from the snapshot while the installed
UsageHub executable remains at the path recorded in that script. It restores
each missing export from its verified encrypted backup, then removes skills
junctions and moves staged originals back, before re-creating the task from
XML, importing Run values with `reg.exe import`, or copying Startup launchers
back. A failed data or skills recovery stops before legacy launchers are
restored; earlier recovery steps may already have completed. A launcher-stage
failure may leave some launchers restored. Inspect the error and partial
state before continuing recovery. Existing exports are never overwritten.
The UTF-16LE `rollback.reg` preserves Unicode and quoted Run command data.

### `migration legacy-backup --path PATH --action … --confirm NAME`

Explicit handling of one legacy plaintext directory. `--confirm` must equal
the directory name. Mutating `encrypt`/`purge` are limited to recognized
Claude Cleaner backup generations (`.claude_backup_*` basenames); anything
else — and anything overlapping broad roots, active account dirs, canonical
provider homes, the shared library, or fork/managed roots in either direction,
or under any reparse point — is refused. `leave` is a read-only
acknowledgement and never deletes.

- `encrypt` → verified encrypted snapshot (`legacy_<sanitized>` account id,
  ASCII alnum/underscore only), then the plaintext generation is REPLACED:
  the recognized dir is purged and the call reports `backup_id` plus
  `files_removed`/`bytes_removed`. The verify step is mandatory: a failed
  verify returns the error and leaves the plaintext untouched. A partial
  purge failure returns the retained backup id and the remaining path
  instead of a false green.
- `purge` → verified recursive delete (refuses when links are inside),
  reporting files/bytes removed.
- `leave` → acknowledged untouched.
- All three are explicit per-invocation choices; the dashboard's legacy panel
  defaults to `encrypt` and requires the directory name to be typed before
  applying.

## Safety contract

- No destructive step without explicit confirmation (`--confirm cutover`,
  `--confirm NAME`/`ID` equality checks).
- Cleanup orchestration (`apply_cleanup`) owns guard → stop → snapshot →
  verify → delete; the CLI only rebuilds a fresh preview and passes the
  confirmed plan through. Cache-only plans need no backup; other scopes
  require all-owned targets plus a verified snapshot unless the user explicitly
  confirms irreversible deletion without a backup.
- The old plaintext generation stays recoverable: encrypted legacy snapshots
  restore to the original legacy directory only, with the same conflict
  checks as account backups plus an explicit plaintext warning.
- `doctor` reuses the read-only preview under its `legacy` key.
