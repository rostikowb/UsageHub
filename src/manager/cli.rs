//! `--manage` noninteractive JSON command surface (integration slice).
//!
//! Placement: called from `main()` before the studio/window stack and the
//! single-instance guard. Read-only commands never touch that mutex; every
//! settings mutation goes through [`state`](super::state) (short registry
//! commits) or the peer-owned manager APIs, which own the per-account guard
//! acquisition per the shared concurrency contract.
//!
//! Envelope: `{"ok":true,"data":{...}}` on stdout, exit 0.
//! Failures: `{"ok":false,"error":{"code":"usage_error"|"operation_error",
//! "message":"..."}}` on stdout plus a one-line stderr diagnostic.
//! Exit codes: `0` ok, `2` usage error, `3` operation error.
//! NEVER `AllocConsole`: plain stdout/stderr so redirected
//! `usagehub.exe --manage ... > out.json` works under `windows_subsystem`.
//!
//! Authoritative usage rule (user override): exact limits AND resets come from
//! the original usageBar server poller ONLY. `poll` calls the live poller
//! read-only (no settings writes, no cache writes, no token refresh prompts);
//! `snapshot` reads the exact bar cache file only. No estimates, no manual
//! resets, no history inference anywhere in this surface.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::process::{clear_inherited_standard_handles, run_bounded, ConsoleError};
use super::{backup, cleanup, lifecycle, migration};
use crate::accounts::{AccountOwnership, AccountProfile, ProviderAccounts, SourceKind};
use crate::app_settings::{self, SettingsFile};
use crate::localization::LanguageId;
use crate::manager::{paths, state, AccountContext};
use crate::models::{UsageData, UsageSection};
use crate::native_interop;
use crate::poller::{self, PollError};
use crate::providers::{ProviderId, ProviderSet};

pub const EXIT_USAGE: i32 = 2;
pub const EXIT_OPERATION: i32 = 3;

/// Entry point wired into `main()`. `None` = no `--manage` flag present.
pub fn handle_manage_mode(args: &[String]) -> Option<i32> {
    let position = args.iter().position(|arg| arg == "--manage")?;
    // Before any spawn: this backend was started with the caller's response
    // pipes as its STD handles, and inherited handles stay inheritable.
    // Rust's Windows launcher passes every inheritable handle to children, so
    // without this a provider CLI or detached descendant could inherit a
    // writer to the caller's pipes and stall its EOF wait. Clears flags only;
    // stdio and the JSON response path are unchanged.
    clear_inherited_standard_handles();
    // Fail fast on relative/empty root overrides before ANY filesystem work.
    if let Err(error) = paths::validate_root_overrides() {
        return Some(emit_fail(Fail::usage(error)));
    }
    let global_args = &args[..position];
    if global_args.iter().any(|arg| arg == "--diagnose") {
        let initialized = if global_args.iter().any(|arg| arg == "--diagnose-append") {
            crate::diagnose::init_append()
        } else {
            crate::diagnose::init()
        };
        if let Err(error) = initialized {
            return Some(emit_fail(Fail::op(format!(
                "Unable to initialize diagnostics: {error}"
            ))));
        }
        // Provider CLI arguments may contain prompts or credentials.
        crate::diagnose::log("management command started");
    }
    Some(match run(&args[position + 1..]) {
        Ok(data) => emit_ok(data),
        Err(fail) => emit_fail(fail),
    })
}

#[derive(Debug)]
enum Fail {
    Usage(String),
    Operation(String),
}

impl Fail {
    fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    fn op(message: impl Into<String>) -> Self {
        Self::Operation(message.into())
    }
    /// Extract the display message, dropping the usage/operation kind.
    /// Used where the caller only carries `String` errors (e.g. the
    /// `state::mutate_settings` closure); the outer `map_err(Fail::op)`
    /// re-kinds the result, so exit codes are unchanged.
    fn message(self) -> String {
        match self {
            Self::Usage(message) | Self::Operation(message) => message,
        }
    }
}

fn emit_ok(data: Value) -> i32 {
    use std::io::Write;
    match writeln!(
        std::io::stdout().lock(),
        "{}",
        json!({"ok": true, "data": data})
    ) {
        Ok(()) => 0,
        Err(error) => emit_fail(Fail::op(format!(
            "Unable to write manager response: {error}"
        ))),
    }
}

fn emit_fail(fail: Fail) -> i32 {
    use std::io::Write;
    let (code, message, exit) = match fail {
        Fail::Usage(message) => ("usage_error", message, EXIT_USAGE),
        Fail::Operation(message) => ("operation_error", message, EXIT_OPERATION),
    };
    // Errors use one machine-readable stderr envelope. The exit code still
    // conveys failure when the caller has no stderr handle (Explorer).
    let _ = writeln!(
        std::io::stderr().lock(),
        "{}",
        json!({"ok": false, "error": {"code": code, "message": message}})
    );
    exit
}

// ---------- argument helpers ----------

/// `--name value` or `--name=value`. Present-without-value is a usage error.
fn flag(tokens: &[String], name: &str) -> Result<Option<String>, Fail> {
    for (index, token) in tokens.iter().enumerate() {
        if token == name {
            return match tokens.get(index + 1) {
                Some(value) if !value.starts_with("--") => Ok(Some(value.clone())),
                _ => Err(Fail::usage(format!("{name} requires a value"))),
            };
        }
        if let Some(value) = token.strip_prefix(&format!("{name}=")) {
            if value.is_empty() {
                return Err(Fail::usage(format!("{name} requires a value")));
            }
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

fn need(value: Option<String>, name: &str) -> Result<String, Fail> {
    value.ok_or_else(|| Fail::usage(format!("{name} is required")))
}

fn has_flag(tokens: &[String], name: &str) -> bool {
    tokens.iter().any(|token| token == name)
}

fn parse_provider(text: &str) -> Result<ProviderId, Fail> {
    match text.to_ascii_lowercase().as_str() {
        "claude" => Ok(ProviderId::Claude),
        "codex" => Ok(ProviderId::Codex),
        other => Err(Fail::usage(format!(
            "unknown provider '{other}'; expected claude or codex"
        ))),
    }
}

fn parse_provider_opt(value: Option<String>) -> Result<Option<ProviderId>, Fail> {
    value.map(|text| parse_provider(&text)).transpose()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

// ---------- registry helpers ----------

fn accounts_of(settings: &SettingsFile, provider: ProviderId) -> Result<&ProviderAccounts, Fail> {
    settings.accounts.get(provider).ok_or_else(|| {
        Fail::op(format!(
            "provider '{}' has no account registry",
            provider.descriptor().key
        ))
    })
}

fn accounts_of_mut(
    settings: &mut SettingsFile,
    provider: ProviderId,
) -> Result<&mut ProviderAccounts, Fail> {
    match provider {
        ProviderId::Claude => Ok(&mut settings.accounts.claude),
        ProviderId::Codex => Ok(&mut settings.accounts.codex),
        _ => Err(Fail::op(format!(
            "provider '{}' has no account registry",
            provider.descriptor().key
        ))),
    }
}

fn stored_profile(provider: ProviderId, id: &str) -> Result<AccountProfile, Fail> {
    let settings = state::load_fresh();
    accounts_of(&settings, provider)?
        .find(id)
        .cloned()
        .ok_or_else(|| {
            Fail::usage(format!(
                "unknown {} account '{id}'",
                provider.descriptor().key
            ))
        })
}

fn ctx_for(provider: ProviderId, id: &str) -> Result<AccountContext, Fail> {
    let profile = stored_profile(provider, id)?;
    AccountContext::from_profile(provider, &profile).map_err(Fail::op)
}

/// Best-effort settings-changed broadcast to a running widget process.
/// Absent window (fresh/smoke use) is silently fine.
fn notify_settings_updated() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW};

    unsafe {
        let class = native_interop::wide_str("UsageHub");
        let Ok(window) = FindWindowW(PCWSTR::from_raw(class.as_ptr()), PCWSTR::null()) else {
            return;
        };
        if window.is_invalid() {
            return;
        }
        let _ = PostMessageW(
            Some(window),
            native_interop::WM_APP_SETTINGS_UPDATED,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

// ---------- dispatch ----------

fn run(tokens: &[String]) -> Result<Value, Fail> {
    let command = tokens
        .first()
        .ok_or_else(|| Fail::usage(format!("missing command; {COMMAND_SUMMARY}")))?;
    let rest = &tokens[1..];
    match command.as_str() {
        "list" => cmd_list(rest),
        "create" => cmd_create(rest),
        "import" => cmd_import(rest),
        "rename" => cmd_rename(rest),
        "open" => cmd_open(rest),
        "signin" => cmd_sign_in(rest),
        "focus" => cmd_focus(rest),
        "status" => cmd_status(rest),
        "stop" => cmd_stop(rest),
        "pin" => cmd_pin(rest),
        "cli" => cmd_provider_cli(tokens),
        "poll" => cmd_poll(rest),
        "snapshot" => cmd_snapshot(rest),
        "cleanup" => cmd_cleanup(rest),
        "backup" => cmd_backup(rest),
        "skills" => cmd_skills(rest),
        "migration" => cmd_migration(rest),
        "doctor" => cmd_doctor(),
        "help" | "--help" | "-h" => Ok(help_text()),
        other => Err(Fail::usage(format!(
            "unknown command '{other}'; {COMMAND_SUMMARY}"
        ))),
    }
}

const COMMAND_SUMMARY: &str = "expected one of: list create import rename open signin focus status stop pin cli poll snapshot cleanup backup skills migration doctor help";

fn help_text() -> Value {
    json!({
        "envelope": {"ok": true, "data": "command payload"},
        "errors": {"code": "usage_error|operation_error", "exit": "2 = usage, 3 = operation"},
        "commands": [
            "list [--provider claude|codex]",
            "create --provider claude|codex --name NAME [--no-open]",
            "import --provider claude|codex --path ABS --name NAME",
            "rename --provider claude|codex --id ID --name NAME",
            "open|focus|status --provider claude|codex --id ID",
            "signin --provider codex --id ID (open an isolated CLI sign-in console)",
            "status [--provider claude|codex] (registry overview when --id is omitted)",
            "stop --provider claude|codex --id ID [--force]",
            "pin --provider claude|codex --id ID --value true|false [--order INTEGER] [--color #RRGGBB]",
            "cli --provider claude|codex --id ID [--timeout-seconds 1..3600] -- [ARGS...] (provider CLI with managed env, captured output; default 120s)",
            "poll --provider claude|codex [--account ID|all] (live read-only server poll)",
            "snapshot --provider claude|codex [--account ID|all] (exact bar cache)",
            "cleanup preview --provider claude|codex --id ID --scope cache|logout|delete|reset",
            "cleanup apply --provider claude|codex --id ID --scope cache|logout|delete|reset --confirm ID [--no-backup]",
            "backup create|list --provider claude|codex [--id ID]",
            "backup verify|restore|purge --backup BACKUP_ID [--confirm BACKUP_ID]",
            "skills import --path ABS [--replace]",
            "migration preview|import|cutover [--confirm cutover]",
            "migration legacy-backup --path PATH --action encrypt|purge|leave --confirm NAME",
            "doctor"
        ]
    })
}

// ---------- account commands ----------

fn source_kind_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::DesktopDir => "desktop_dir",
        SourceKind::CredentialFile => "credential_file",
        SourceKind::ProviderDefault => "provider_default",
    }
}

fn listed_providers(filter: Option<ProviderId>) -> Vec<ProviderId> {
    match filter {
        Some(provider) => vec![provider],
        None => vec![ProviderId::Claude, ProviderId::Codex],
    }
}

fn profile_json(provider: ProviderId, profile: &AccountProfile, settings: &SettingsFile) -> Value {
    let accounts = accounts_of(settings, provider).ok();
    json!({
        "id": profile.id,
        "name": profile.name,
        "enabled": profile.enabled,
        "ownership": serde_json::to_value(profile.ownership).unwrap_or(Value::Null),
        "pinned": profile.pinned,
        "sort_order": profile.sort_order,
        "color": profile.color,
        "label": profile.label,
        "selected": accounts.is_some_and(|list| list.selected == profile.id),
        "source": source_kind_name(profile.source_kind(provider)),
        "desktop_dir": profile.desktop_dir,
        "config_dir": profile.config_dir,
        "credential_targets": profile.credential_targets,
    })
}

fn cmd_list(tokens: &[String]) -> Result<Value, Fail> {
    let filter = parse_provider_opt(flag(tokens, "--provider")?)?;
    let settings = state::load_fresh();
    let mut entries = Vec::new();
    for provider in listed_providers(filter) {
        let accounts = accounts_of(&settings, provider)?;
        for profile in &accounts.profiles {
            entries.push(json!({
                "provider": provider.descriptor().key,
                "account": profile_json(provider, profile, &settings),
            }));
        }
    }
    Ok(json!({"accounts": entries}))
}

fn cmd_create(tokens: &[String]) -> Result<Value, Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let name = need(flag(tokens, "--name")?, "--name")?;
    if name.trim().is_empty() {
        return Err(Fail::usage("--name must not be empty"));
    }
    let opts = lifecycle::NewAccountOpts {
        desktop_dir: None,
        config_dir: None,
        ownership: AccountOwnership::Managed,
        ..Default::default()
    };
    let profile = lifecycle::create_account(provider, &name, opts).map_err(Fail::op)?;
    let mut opened = false;
    if !has_flag(tokens, "--no-open") {
        lifecycle::open_account(provider, &profile.id).map_err(|error| {
            Fail::op(format!(
                "account '{}' created but open failed: {error}",
                profile.id
            ))
        })?;
        opened = true;
    }
    notify_settings_updated();
    Ok(json!({
        "provider": provider.descriptor().key,
        "created": serde_json::to_value(&profile).unwrap_or(Value::Null),
        "opened": opened,
    }))
}

fn cmd_import(tokens: &[String]) -> Result<Value, Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let raw = need(flag(tokens, "--path")?, "--path")?;
    let path = paths::require_absolute(Path::new(&raw)).map_err(Fail::usage)?;
    if !path.exists() {
        return Err(Fail::usage(format!(
            "--path does not exist: '{}'",
            path.display()
        )));
    }
    let name = need(flag(tokens, "--name")?, "--name")?;
    let opts = lifecycle::ImportOpts {
        name: Some(name),
        ..Default::default()
    };
    let profile = lifecycle::import_account(provider, &path, opts).map_err(Fail::op)?;
    notify_settings_updated();
    Ok(json!({
        "provider": provider.descriptor().key,
        "imported": serde_json::to_value(&profile).unwrap_or(Value::Null),
    }))
}

fn cmd_rename(tokens: &[String]) -> Result<Value, Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let id = need(flag(tokens, "--id")?, "--id")?;
    let name = need(flag(tokens, "--name")?, "--name")?;
    lifecycle::rename_account(provider, &id, &name).map_err(Fail::op)?;
    notify_settings_updated();
    Ok(json!({"provider": provider.descriptor().key, "id": id, "renamed": true}))
}

fn cmd_open(tokens: &[String]) -> Result<Value, Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let id = need(flag(tokens, "--id")?, "--id")?;
    if let Err(message) = lifecycle::open_account(provider, &id) {
        if has_flag(tokens, "--from-shortcut") {
            super::native::show_error_dialog("UsageHub", &message);
        }
        return Err(Fail::op(message));
    }
    Ok(json!({"provider": provider.descriptor().key, "id": id, "opened": true}))
}

fn cmd_sign_in(tokens: &[String]) -> Result<Value, Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let id = need(flag(tokens, "--id")?, "--id")?;
    lifecycle::sign_in_account(provider, &id).map_err(Fail::op)?;
    Ok(json!({"provider": provider.descriptor().key, "id": id, "console_opened": true}))
}

fn cmd_focus(tokens: &[String]) -> Result<Value, Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let id = need(flag(tokens, "--id")?, "--id")?;
    let focused = lifecycle::focus_account_window(provider, &id).map_err(Fail::op)?;
    Ok(json!({"provider": provider.descriptor().key, "id": id, "focused": focused}))
}

fn cmd_status(tokens: &[String]) -> Result<Value, Fail> {
    let provider_opt = parse_provider_opt(flag(tokens, "--provider")?)?;
    let id_opt = flag(tokens, "--id")?;
    match (provider_opt, id_opt) {
        (Some(provider), Some(id)) => {
            let report = lifecycle::account_status(provider, &id).map_err(Fail::op)?;
            Ok(json!({
                "provider": provider.descriptor().key,
                "account_id": id,
                "status": serde_json::to_value(&report).unwrap_or(Value::Null),
            }))
        }
        // Registry overview (no live poll); mirrors `list`.
        _ => cmd_list(tokens),
    }
}

fn cmd_stop(tokens: &[String]) -> Result<Value, Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let id = need(flag(tokens, "--id")?, "--id")?;
    // Cleanup already owns this guard when calling the low-level stop helper.
    // A standalone stop must also exclude launches, restores, and CLI jobs.
    let _account_guard =
        state::AccountOperationGuard::acquire(provider, &id, 5_000).map_err(Fail::op)?;
    let forced = has_flag(tokens, "--force");
    let report = if forced {
        lifecycle::force_stop_account_processes(provider, &id).map_err(Fail::op)?
    } else {
        lifecycle::stop_account_processes(provider, &id).map_err(Fail::op)?
    };
    if !report.verified_idle {
        let detail = serde_json::to_string(&report)
            .map_err(|error| Fail::op(format!("Unable to encode stop result: {error}")))?;
        return Err(Fail::op(format!(
            "Account '{id}' is not fully stopped: {detail}"
        )));
    }
    notify_settings_updated();
    Ok(json!({
        "provider": provider.descriptor().key,
        "id": id,
        "forced": forced,
        "report": serde_json::to_value(&report).unwrap_or(Value::Null),
    }))
}

fn cmd_pin(tokens: &[String]) -> Result<Value, Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let id = need(flag(tokens, "--id")?, "--id")?;
    let raw_value = need(flag(tokens, "--value")?, "--value")?;
    let pinned = match raw_value.to_ascii_lowercase().as_str() {
        "true" => true,
        "false" => false,
        _ => {
            return Err(Fail::usage(format!(
                "--value must be true or false, got '{raw_value}'"
            )));
        }
    };
    let order = match flag(tokens, "--order")? {
        Some(raw) => Some(
            raw.parse::<i32>()
                .map_err(|_| Fail::usage(format!("--order must be an integer, got '{raw}'")))?,
        ),
        None => None,
    };
    let color = flag(tokens, "--color")?;
    if let Some(color) = &color {
        if !color.is_empty()
            && (color.len() != 7
                || !color.starts_with('#')
                || !color.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit))
        {
            return Err(Fail::usage(
                "--color must be #RRGGBB or empty to use the default",
            ));
        }
    }
    // Display-only registry commit under the short settings guard.
    let profile = state::mutate_settings(|settings| {
        let accounts = accounts_of_mut(settings, provider).map_err(Fail::message)?;
        let entry = accounts
            .find_mut(&id)
            .ok_or_else(|| format!("Unknown account '{id}'"))?;
        entry.pinned = pinned;
        if let Some(sort_order) = order {
            entry.sort_order = sort_order;
        }
        if let Some(color) = &color {
            entry.color = color.to_ascii_lowercase();
        }
        Ok(entry.clone())
    })
    .map_err(Fail::op)?;
    notify_settings_updated();
    Ok(json!({
        "provider": provider.descriptor().key,
        "account": serde_json::to_value(&profile).unwrap_or(Value::Null),
    }))
}

// ---------- provider CLI passthrough ----------

/// Max captured bytes per stream; beyond that the output is truncated.
/// Guards the JSON envelope against runaway provider output.
const MAX_CAPTURE_BYTES: usize = 256 * 1024;

/// Default bound for one explicit provider CLI invocation.
const DEFAULT_CLI_TIMEOUT: Duration = Duration::from_secs(120);
/// Inclusive `--timeout-seconds` bounds for `cli`, parsed before `--`.
const MIN_CLI_TIMEOUT_SECS: u64 = 1;
const MAX_CLI_TIMEOUT_SECS: u64 = 3600;

/// Explicit `--timeout-seconds N` before the CLI `--` separator (1..=3600,
/// default 120s). Only flags preceding `--` are inspected so passthrough
/// args after the separator are never mistaken for options.
fn provider_cli_timeout(flags: &[String]) -> Result<Duration, Fail> {
    match flag(flags, "--timeout-seconds")? {
        None => Ok(DEFAULT_CLI_TIMEOUT),
        Some(raw) => {
            let secs: u64 = raw.parse().map_err(|_| {
                Fail::usage(format!(
                    "--timeout-seconds must be an integer 1..=3600, got '{raw}'"
                ))
            })?;
            if !(MIN_CLI_TIMEOUT_SECS..=MAX_CLI_TIMEOUT_SECS).contains(&secs) {
                return Err(Fail::usage(format!(
                    "--timeout-seconds must be 1..=3600, got '{raw}'"
                )));
            }
            Ok(Duration::from_secs(secs))
        }
    }
}

fn cmd_provider_cli(full_tokens: &[String]) -> Result<Value, Fail> {
    // `full_tokens[0]` is "cli"; provider flags precede `--`, CLI args follow.
    let separator = full_tokens.iter().position(|token| token == "--");
    let (flags, cli_args) = match separator {
        Some(index) => (&full_tokens[1..index], &full_tokens[index + 1..]),
        None => (&full_tokens[1..], &[][..]),
    };
    if cli_args.is_empty() {
        return Err(Fail::usage(
            "provide provider CLI args after `--`, e.g. cli --provider claude --id ID -- --version",
        ));
    }
    let provider = parse_provider(&need(flag(flags, "--provider")?, "--provider")?)?;
    let id = need(flag(flags, "--id")?, "--id")?;
    let timeout = provider_cli_timeout(flags)?;
    // Held across execution so a concurrent stop/launch cannot interleave;
    // released on return, including timeout kills.
    let _account_guard =
        state::AccountOperationGuard::acquire(provider, &id, 5_000).map_err(Fail::op)?;
    let context = ctx_for(provider, &id)?;
    let env = lifecycle::account_cli_env(&context).map_err(Fail::op)?;
    let binary_name = match provider {
        ProviderId::Claude => "claude",
        ProviderId::Codex => "codex",
        _ => {
            return Err(Fail::op(format!(
                "provider '{}' has no CLI passthrough",
                provider.descriptor().key
            )));
        }
    };
    let binary = resolve_cli_binary(binary_name).ok_or_else(|| {
        Fail::op(format!(
            "{binary_name} executable not found on PATH; install the provider CLI first"
        ))
    })?;
    // Rust's Windows process launcher recognizes .cmd/.bat and applies its
    // batch-file escaping when the literal path is handed to Command
    // directly. A hand-written `cmd /C` wrapper would bypass it.
    let mut command = std::process::Command::new(&binary);
    command.args(cli_args);
    command.envs(env.iter().map(|(key, value)| (key, value)));
    // No token material is exported: env comes from lifecycle::account_cli_env.
    // run_bounded owns stdio null/pipes, the hidden console, the 256KiB cap
    // and the kill-on-timeout job tree. No raw CLI args in errors
    // (prompts/auth may ride along); timeout surfaces as operation_error.
    let output =
        run_bounded(&mut command, timeout, MAX_CAPTURE_BYTES, "provider CLI").map_err(|error| {
            match error {
                ConsoleError::TimedOut { timeout, .. } => Fail::op(format!(
                    "Provider CLI timed out after {}s",
                    timeout.as_secs()
                )),
                ConsoleError::Failed { detail, .. } => Fail::op(detail),
            }
        })?;
    let (stdout, stdout_truncated) = truncate_capture(&output.stdout);
    let (stderr, stderr_truncated) = truncate_capture(&output.stderr);
    Ok(json!({
        "provider": provider.descriptor().key,
        "id": id,
        "binary": binary.display().to_string(),
        "exit_code": output.status.code(),
        "stdout": stdout,
        "stderr": stderr,
        "truncated": stdout_truncated || stderr_truncated,
    }))
}

fn resolve_cli_binary(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    // Prefer real executables over shell shims so output capture is exact.
    let candidates: Vec<PathBuf> = ["exe", "cmd", "bat", ""]
        .into_iter()
        .map(|extension| {
            if extension.is_empty() {
                PathBuf::from(name)
            } else {
                PathBuf::from(format!("{name}.{extension}"))
            }
        })
        .collect();
    for directory in std::env::split_paths(&path_var) {
        for candidate in &candidates {
            let full = directory.join(candidate);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

fn truncate_capture(bytes: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    if text.len() > MAX_CAPTURE_BYTES {
        let mut cut = MAX_CAPTURE_BYTES;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        (format!("{}…[truncated]", &text[..cut]), true)
    } else {
        (text.into_owned(), false)
    }
}

// ---------- poll / snapshot (read-only authoritative usage) ----------

fn system_unix(time: Option<SystemTime>) -> Option<u64> {
    time.and_then(|instant| {
        instant
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|elapsed| elapsed.as_secs())
    })
}

/// Absolute server reset instant as ISO-8601 UTC (`null` = unknown).
/// Hand-rolled civil-date math: no chrono dependency, exact timestamps only.
fn unix_to_iso8601(secs: u64) -> String {
    let days = secs / 86_400;
    let clock = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        clock / 3_600,
        (clock % 3_600) / 60,
        clock % 60
    )
}

fn civil_from_days(days: u64) -> (i64, u32, u32) {
    // Howard Hinnant's days-to-civil algorithm, valid for non-negative days.
    let absolute = days as i64 + 719_468;
    let era = absolute.div_euclid(146_097);
    let day_of_era = absolute - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

fn section_json(section: &UsageSection) -> Value {
    let resets_at = system_unix(section.resets_at);
    json!({
        "available": section.available,
        // Absent windows carry no figure: null, never 0.
        "percentage": section.available.then_some(section.percentage),
        "resets_at_unix": resets_at,
        "resets_at": resets_at.map(unix_to_iso8601),
    })
}

fn poll_error_json(error: PollError) -> Value {
    json!({
        "kind": serde_json::to_value(&error).unwrap_or(Value::Null),
        "auth": error.is_auth(),
        "transient": error.is_transient(),
        "message": error.message(LanguageId::English),
    })
}

#[allow(clippy::too_many_arguments)]
fn account_entry(
    provider: ProviderId,
    profile: &AccountProfile,
    usage: Option<&UsageData>,
    error: Option<PollError>,
    source_path: Option<&Path>,
    source_signature: &str,
    selected: bool,
    observed_unix: u64,
) -> Value {
    let (usage_json, resets_json) = match usage {
        Some(data) => (
            json!({
                "session": section_json(&data.session),
                "session_label": data.session_label,
                "weekly": section_json(&data.weekly),
                "weekly_label": data.weekly_label,
                "monthly": data.monthly.as_ref().map(section_json),
                "credits": data.credits.as_ref().map(|credits| json!({
                    "percentage": credits.percentage,
                    "remaining": credits.remaining,
                    "total": credits.total,
                })),
                "limits": data.limits.iter().map(|limit| json!({
                    "key": limit.key,
                    "kind": limit.kind,
                    "label": limit.label,
                    "model": limit.model,
                    "model_id": limit.model_id,
                    "scope": limit.scope,
                    "is_active": limit.is_active,
                    "usage": section_json(&limit.usage),
                })).collect::<Vec<_>>(),
                "stale": data.stale,
            }),
            json!({
                "session": system_unix(data.session.resets_at).map(unix_to_iso8601),
                "weekly": system_unix(data.weekly.resets_at).map(unix_to_iso8601),
            }),
        ),
        None => (
            Value::Null,
            json!({"session": Value::Null, "weekly": Value::Null}),
        ),
    };
    json!({
        "provider": provider.descriptor().key,
        "account_id": profile.id,
        "name": profile.name,
        "enabled": profile.enabled,
        "selected": selected,
        "observed_unix": observed_unix,
        "observed_at": unix_to_iso8601(observed_unix),
        "source": {
            "kind": source_kind_name(profile.source_kind(provider)),
            "path": source_path.map(|path| path.display().to_string()),
            "signature": source_signature,
        },
        "usage": usage_json,
        "resets": resets_json,
        "error": error.map(poll_error_json),
    })
}

fn filter_account(id_filter: Option<&str>, id: &str) -> bool {
    match id_filter {
        None | Some("all") => true,
        Some(wanted) => wanted == id,
    }
}

fn cmd_poll(tokens: &[String]) -> Result<Value, Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let account_filter = flag(tokens, "--account")?;
    let settings = state::load_fresh();
    // Unknown/disabled account ids are usage errors, reported before polling.
    if let Some(wanted) = account_filter.as_deref() {
        if wanted != "all" {
            let known = accounts_of(&settings, provider)?
                .find(wanted)
                .filter(|profile| profile.enabled);
            if known.is_none() {
                return Err(Fail::usage(format!(
                    "unknown or disabled {} account '{wanted}'",
                    provider.descriptor().key
                )));
            }
        }
    }
    // Live read-only server poll. force=false: auth-paused sources stay
    // paused, no manual refresh, no settings or cache writes here.
    let providers = ProviderSet::from_enabled([provider]);
    let data =
        poller::poll(providers, &settings.accounts, None, false, |_| {}).map_err(|failure| {
            Fail::op(format!(
                "{} poll failed: {}",
                failure.provider.descriptor().key,
                failure.error.message(LanguageId::English)
            ))
        })?;
    let observed = now_unix();
    let entries: Vec<Value> = data
        .accounts
        .iter()
        .filter(|account| {
            account.provider == provider
                && filter_account(account_filter.as_deref(), &account.profile.id)
        })
        .map(|account| {
            account_entry(
                account.provider,
                &account.profile,
                account.usage.as_ref(),
                account.error,
                account.source_path.as_deref(),
                &account.source_signature,
                account.selected,
                observed,
            )
        })
        .collect();
    Ok(
        json!({"provider": provider.descriptor().key, "observed_unix": observed, "accounts": entries}),
    )
}

fn cmd_snapshot(tokens: &[String]) -> Result<Value, Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let account_filter = flag(tokens, "--account")?;
    // Exact bar cache only: timestamped readings with stale flags, no estimates.
    let cache = app_settings::load_usage_cache().ok_or_else(|| {
        Fail::op("no cached usage yet; run `--manage poll` first or wait for a poll cycle")
    })?;
    let settings = state::load_fresh();
    let mut data = cache.data.clone();
    data.select_accounts(&settings.accounts);
    let entries: Vec<Value> = data
        .accounts
        .iter()
        .filter(|account| {
            account.provider == provider
                && filter_account(account_filter.as_deref(), &account.profile.id)
        })
        .map(|account| {
            account_entry(
                account.provider,
                &account.profile,
                account.usage.as_ref(),
                account.error,
                account.source_path.as_deref(),
                &account.source_signature,
                account.selected,
                cache.updated_unix,
            )
        })
        .collect();
    Ok(json!({
        "provider": provider.descriptor().key,
        "cache_updated_unix": cache.updated_unix,
        "cache_updated_at": unix_to_iso8601(cache.updated_unix),
        "cache_poll_ok": cache.poll_ok,
        "accounts": entries,
    }))
}

// ---------- cleanup / backup / skills ----------

fn cmd_cleanup(tokens: &[String]) -> Result<Value, Fail> {
    let action = tokens
        .first()
        .ok_or_else(|| Fail::usage("cleanup requires preview or apply"))?;
    match action.as_str() {
        "preview" => cmd_cleanup_preview(&tokens[1..]),
        "apply" => cmd_cleanup_apply(&tokens[1..]),
        other => Err(Fail::usage(format!(
            "unknown cleanup action '{other}'; expected preview or apply"
        ))),
    }
}

fn cleanup_context(tokens: &[String]) -> Result<(ProviderId, String, AccountContext), Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let id = need(flag(tokens, "--id")?, "--id")?;
    let context = ctx_for(provider, &id)?;
    Ok((provider, id, context))
}

fn cmd_cleanup_preview(tokens: &[String]) -> Result<Value, Fail> {
    let (provider, id, context) = cleanup_context(tokens)?;
    // scope_from_cli_name is the single owner of the cache|logout|delete|reset spelling.
    let scope = cleanup::scope_from_cli_name(&need(flag(tokens, "--scope")?, "--scope")?)
        .map_err(Fail::usage)?;
    // Strictly read-only: no guard, no mutation, no backup.
    let plan = cleanup::preview_cleanup(&context, scope).map_err(Fail::op)?;
    Ok(json!({
        "provider": provider.descriptor().key,
        "account_id": id,
        "plan": serde_json::to_value(&plan).unwrap_or(Value::Null),
    }))
}

fn cmd_cleanup_apply(tokens: &[String]) -> Result<Value, Fail> {
    let (provider, id, context) = cleanup_context(tokens)?;
    let scope = cleanup::scope_from_cli_name(&need(flag(tokens, "--scope")?, "--scope")?)
        .map_err(Fail::usage)?;
    let confirm = need(flag(tokens, "--confirm")?, "--confirm")?;
    if confirm != id {
        return Err(Fail::usage(format!(
            "--confirm '{confirm}' does not match --id '{id}'; refusing destructive apply"
        )));
    }
    // Rebuild the preview fresh from the registry in this same invocation,
    // then hand orchestration (guard, stop, snapshot, verify, delete) to
    // cleanup::apply_cleanup. CLI creates NO backup here by design.
    let plan = cleanup::preview_cleanup(&context, scope).map_err(Fail::op)?;
    let result = if has_flag(tokens, "--no-backup") {
        cleanup::apply_cleanup_irreversible(&plan)
    } else {
        cleanup::apply_cleanup(&plan)
    };
    // Even an error can follow a committed epoch or a partially committed
    // registry/cache update, so the monitor must always reload after apply.
    notify_settings_updated();
    let report = result.map_err(Fail::op)?;
    Ok(json!({
        "provider": provider.descriptor().key,
        "account_id": id,
        "report": serde_json::to_value(&report).unwrap_or(Value::Null),
    }))
}

fn cmd_backup(tokens: &[String]) -> Result<Value, Fail> {
    let action = tokens
        .first()
        .ok_or_else(|| Fail::usage("backup requires create, list, verify, restore or purge"))?;
    match action.as_str() {
        "create" => cmd_backup_create(&tokens[1..]),
        "list" => cmd_backup_list(&tokens[1..]),
        "verify" => cmd_backup_verify(&tokens[1..]),
        "restore" => cmd_backup_restore(&tokens[1..]),
        "purge" => cmd_backup_purge(&tokens[1..]),
        other => Err(Fail::usage(format!(
            "unknown backup action '{other}'; expected create, list, verify, restore or purge"
        ))),
    }
}

fn cmd_backup_create(tokens: &[String]) -> Result<Value, Fail> {
    let provider = parse_provider(&need(flag(tokens, "--provider")?, "--provider")?)?;
    let id = need(flag(tokens, "--id")?, "--id")?;
    let context = ctx_for(provider, &id)?;
    let info = backup::create_backup(&context).map_err(Fail::op)?;
    notify_settings_updated();
    Ok(json!({"backup": serde_json::to_value(&info).unwrap_or(Value::Null)}))
}

fn cmd_backup_list(tokens: &[String]) -> Result<Value, Fail> {
    let filter = parse_provider_opt(flag(tokens, "--provider")?)?;
    let id_filter = flag(tokens, "--id")?;
    let mut infos = backup::list_all_backups().map_err(Fail::op)?;
    infos.retain(|info| {
        filter.is_none_or(|provider| info.provider == provider)
            && id_filter.as_deref().is_none_or(|id| info.account_id == id)
    });
    Ok(json!({"backups": infos}))
}

fn backup_id_flag(tokens: &[String]) -> Result<String, Fail> {
    need(flag(tokens, "--backup")?, "--backup")
}

fn cmd_backup_verify(tokens: &[String]) -> Result<Value, Fail> {
    let backup_id = backup_id_flag(tokens)?;
    let info = backup::verify_backup(&backup_id).map_err(Fail::op)?;
    Ok(json!({"backup": serde_json::to_value(&info).unwrap_or(Value::Null)}))
}

fn cmd_backup_restore(tokens: &[String]) -> Result<Value, Fail> {
    let backup_id = backup_id_flag(tokens)?;
    let confirm = need(flag(tokens, "--confirm")?, "--confirm")?;
    if confirm != backup_id {
        return Err(Fail::usage(format!(
            "--confirm '{confirm}' does not match --backup '{backup_id}'; refusing restore"
        )));
    }
    backup::restore_backup(&backup_id).map_err(Fail::op)?;
    notify_settings_updated();
    Ok(json!({"restored": backup_id}))
}

fn cmd_backup_purge(tokens: &[String]) -> Result<Value, Fail> {
    let backup_id = backup_id_flag(tokens)?;
    let confirm = need(flag(tokens, "--confirm")?, "--confirm")?;
    if confirm != backup_id {
        return Err(Fail::usage(format!(
            "--confirm '{confirm}' does not match --backup '{backup_id}'; refusing purge"
        )));
    }
    backup::purge_backup(&backup_id).map_err(Fail::op)?;
    notify_settings_updated();
    Ok(json!({"purged": backup_id}))
}

fn cmd_skills(tokens: &[String]) -> Result<Value, Fail> {
    let action = tokens
        .first()
        .ok_or_else(|| Fail::usage("skills requires import"))?;
    if action.as_str() != "import" {
        return Err(Fail::usage(format!(
            "unknown skills action '{action}'; expected import"
        )));
    }
    cmd_skills_import(&tokens[1..])
}

fn cmd_skills_import(tokens: &[String]) -> Result<Value, Fail> {
    use crate::manager::skills;

    let raw = need(flag(tokens, "--path")?, "--path")?;
    // Absolute-only is a usage error (exit 2); every other Err is operational.
    let source = paths::require_absolute(Path::new(&raw)).map_err(Fail::usage)?;
    if !source.is_dir() {
        return Err(Fail::usage(format!(
            "--path must be a skill directory: '{}'",
            source.display()
        )));
    }
    let replace = has_flag(tokens, "--replace");
    // Canonical import lives in manager::skills (conflict preview, no-replace
    // never touches existing bytes, replace removes-then-copies conflicts
    // only, source links refused never followed). Forwarded opaquely.
    let report = skills::import_skills_from(&source, replace).map_err(Fail::op)?;
    notify_settings_updated();
    Ok(json!({
        "report": serde_json::to_value(&report).unwrap_or(Value::Null),
    }))
}

// ---------- migration ----------

fn cmd_migration(tokens: &[String]) -> Result<Value, Fail> {
    let action = tokens.first().ok_or_else(|| {
        Fail::usage("migration requires preview, import, cutover or legacy-backup")
    })?;
    match action.as_str() {
        "preview" => migration::preview().map_err(Fail::op),
        "import" => {
            let data = migration::adopt().map_err(Fail::op)?;
            notify_settings_updated();
            Ok(data)
        }
        "cutover" => {
            let confirm = need(flag(tokens, "--confirm")?, "--confirm")?;
            if confirm != "cutover" {
                return Err(Fail::usage(
                    "--confirm cutover is required; run `migration preview` first",
                ));
            }
            migration::cutover().map_err(Fail::op)
        }
        "legacy-backup" => cmd_legacy_backup(&tokens[1..]),
        other => Err(Fail::usage(format!(
            "unknown migration action '{other}'; expected preview, import, cutover or legacy-backup"
        ))),
    }
}

fn cmd_legacy_backup(tokens: &[String]) -> Result<Value, Fail> {
    let raw = need(flag(tokens, "--path")?, "--path")?;
    let action = need(flag(tokens, "--action")?, "--action")?;
    if !["encrypt", "purge", "leave"].contains(&action.as_str()) {
        return Err(Fail::usage(format!(
            "unknown --action '{action}'; expected encrypt, purge or leave"
        )));
    }
    let confirm = need(flag(tokens, "--confirm")?, "--confirm")?;
    let file_name = Path::new(&raw)
        .file_name()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| Fail::usage("--path has no usable directory name"))?;
    if !file_name.eq_ignore_ascii_case(&confirm) {
        return Err(Fail::usage(format!(
            "--confirm '{confirm}' does not match directory '{file_name}'; refusing legacy-backup {action}"
        )));
    }
    let data = migration::legacy_backup(Path::new(&raw), &action).map_err(Fail::op)?;
    notify_settings_updated();
    Ok(data)
}

// ---------- doctor ----------

fn cmd_doctor() -> Result<Value, Fail> {
    let config_root = paths::fork_config_dir().map(|path| path.display().to_string());
    let local_root = paths::fork_local_dir().map(|path| path.display().to_string());
    let profiles_root = paths::profiles_root().map(|path| path.display().to_string());
    let settings_path = app_settings::settings_path();
    let settings_exists = settings_path.is_file();
    let settings = state::load_fresh();
    let mut account_entries = Vec::new();
    for provider in [ProviderId::Claude, ProviderId::Codex] {
        let Ok(accounts) = accounts_of(&settings, provider) else {
            continue;
        };
        for profile in &accounts.profiles {
            let context = AccountContext::from_profile(provider, profile);
            account_entries.push(json!({
                "provider": provider.descriptor().key,
                "id": profile.id,
                "name": profile.name,
                "enabled": profile.enabled,
                "source": source_kind_name(profile.source_kind(provider)),
                "desktop_dir": profile.desktop_dir,
                "config_dir": profile.config_dir,
                "context_error": context.err().map(|error| error.to_string()),
            }));
        }
    }
    let guard_state = match state::SettingsOperationGuard::acquire(0) {
        Ok(_guard) => "free",
        Err(error) => {
            if error.contains("Timed out") {
                "busy"
            } else {
                "error"
            }
        }
    };
    let legacy = match migration::preview() {
        Ok(data) => data,
        Err(error) => json!({"error": error}),
    };
    Ok(json!({
        "roots": {
            "config": config_root,
            "local": local_root,
            "profiles": profiles_root,
        },
        "settings": {
            "path": settings_path.display().to_string(),
            "exists": settings_exists,
            "claude_accounts": settings.accounts.claude.profiles.len(),
            "codex_accounts": settings.accounts.codex.profiles.len(),
        },
        "themes": {
            "dir": crate::theme_engine::themes_directory().display().to_string(),
            "exists": crate::theme_engine::themes_directory().is_dir(),
            "active": settings.active_theme_path,
        },
        "accounts": account_entries,
        "instance": {
            "settings_guard": guard_state,
        },
        "legacy": legacy,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_parse_case_insensitively() {
        assert_eq!(parse_provider("claude").unwrap(), ProviderId::Claude);
        assert_eq!(parse_provider("Codex").unwrap(), ProviderId::Codex);
        assert!(parse_provider("gemini").is_err());
    }

    #[test]
    fn flag_forms_and_missing_values() {
        let tokens = vec!["--id".to_string(), "abc".to_string()];
        assert_eq!(flag(&tokens, "--id").unwrap(), Some("abc".to_string()));
        let tokens = vec!["--id=abc".to_string()];
        assert_eq!(flag(&tokens, "--id").unwrap(), Some("abc".to_string()));
        let tokens = vec!["--id".to_string()];
        assert!(flag(&tokens, "--id").is_err());
        let tokens = vec!["--id".to_string(), "--force".to_string()];
        assert!(flag(&tokens, "--id").is_err());
        assert!(has_flag(&tokens, "--force"));
        assert!(!has_flag(&tokens, "--nope"));
    }

    #[test]
    fn iso8601_known_instants() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(unix_to_iso8601(86_400), "1970-01-02T00:00:00Z");
        assert_eq!(unix_to_iso8601(31_536_000), "1971-01-01T00:00:00Z");
        // 2020-02-29 (leap day): 2020-01-01 + 59 days, plus 1h1m1s.
        assert_eq!(unix_to_iso8601(1_582_934_400), "2020-02-29T00:00:00Z");
        assert_eq!(
            unix_to_iso8601(1_582_934_400 + 3_661),
            "2020-02-29T01:01:01Z"
        );
    }

    #[test]
    fn unavailable_windows_serialize_without_figures() {
        let section = UsageSection {
            available: false,
            percentage: 99.0,
            resets_at: None,
        };
        let value = section_json(&section);
        assert_eq!(value["available"], false);
        assert!(value["percentage"].is_null());
        assert!(value["resets_at"].is_null());
    }

    #[test]
    fn cli_timeout_option_bounds() {
        let none: Vec<String> = vec![];
        assert_eq!(provider_cli_timeout(&none).unwrap(), DEFAULT_CLI_TIMEOUT);
        for raw in ["1", "120", "3600"] {
            let flags = vec![format!("--timeout-seconds={raw}")];
            assert_eq!(
                provider_cli_timeout(&flags).unwrap(),
                Duration::from_secs(raw.parse().unwrap())
            );
        }
        let spaced = vec!["--timeout-seconds".to_string(), "60".to_string()];
        assert_eq!(
            provider_cli_timeout(&spaced).unwrap(),
            Duration::from_secs(60)
        );
        for raw in ["0", "3601", "abc", ""] {
            let flags = vec![format!("--timeout-seconds={raw}")];
            assert!(
                matches!(provider_cli_timeout(&flags), Err(Fail::Usage(_))),
                "--timeout-seconds={raw} must be a usage error"
            );
        }
        let bare = vec!["--timeout-seconds".to_string()];
        assert!(matches!(provider_cli_timeout(&bare), Err(Fail::Usage(_))));
    }
}
