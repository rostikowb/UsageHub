// Keep the complete translation catalogue while the GPU dashboard progressively
// adopts the legacy widget strings.
#![allow(dead_code)]

use windows::core::PWSTR;
use windows::Win32::Globalization::{
    GetUserDefaultLocaleName, GetUserDefaultUILanguage, GetUserPreferredUILanguages,
    LCIDToLocaleName, LOCALE_ALLOW_NEUTRAL_NAMES, MAX_LOCALE_NAME, MUI_LANGUAGE_NAME,
};

use crate::providers::ProviderId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanguageId(usize);

struct Locale {
    code: &'static str,
    native_name: &'static str,
    locale_patterns: &'static [&'static str],
    windows_font: Option<(&'static str, &'static str)>,
    update_via_winget_label: &'static str,
    strings: Strings,
    translations: &'static [(&'static str, &'static str)],
}

mod generated {
    use super::{LanguageId, Locale, Strings};

    include!(concat!(env!("OUT_DIR"), "/locales.rs"));
}

impl LanguageId {
    #[allow(non_upper_case_globals)]
    pub const English: Self = Self(generated::ENGLISH_INDEX);

    pub const ALL: [Self; generated::LANGUAGE_COUNT] = generated::LANGUAGE_IDS;

    fn locale(self) -> &'static Locale {
        &generated::LOCALES[self.0]
    }

    pub fn code(self) -> &'static str {
        self.locale().code
    }

    pub fn native_name(self) -> &'static str {
        self.locale().native_name
    }

    pub fn strings(self) -> Strings {
        self.locale().strings
    }

    /// Translate user-interface text introduced by the dashboard and Theme Studio.
    ///
    /// English text is used as the stable catalogue key. Locale modules may
    /// deliberately fall back to that key while a specialist term is awaiting
    /// a reviewed translation.
    pub fn text(self, english: &'static str) -> &'static str {
        let locale = self.locale();
        locale
            .translations
            .binary_search_by(|(key, _)| (*key).cmp(english))
            .map(|index| locale.translations[index].1)
            .unwrap_or(english)
    }

    pub fn update_via_winget_label(self) -> &'static str {
        self.locale().update_via_winget_label
    }

    pub fn provider_auth_error(self, provider: ProviderId) -> (&'static str, &'static str) {
        let strings = self.strings();
        match provider {
            ProviderId::Claude => (
                strings.token_expired_title,
                self.text("Sign in again with the Claude desktop app or run 'claude' and use '/login' for your CLI account. Then refresh this monitor."),
            ),
            ProviderId::Codex => (
                strings.codex_token_expired_title,
                strings.codex_token_expired_body,
            ),
            ProviderId::Antigravity => (
                strings.antigravity_token_expired_title,
                strings.antigravity_token_expired_body,
            ),
            ProviderId::OpenCode => (
                strings.opencode_token_expired_title,
                strings.opencode_token_expired_body,
            ),
            ProviderId::Cursor => (
                strings.cursor_token_expired_title,
                strings.cursor_token_expired_body,
            ),
            ProviderId::Grok => (
                strings.grok_token_expired_title,
                strings.grok_token_expired_body,
            ),
            ProviderId::Copilot => (
                strings.copilot_token_expired_title,
                strings.copilot_token_expired_body,
            ),
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        let normalized = code.trim().replace('_', "-").to_ascii_lowercase();
        if normalized.is_empty() || normalized == "system" {
            return None;
        }

        Self::ALL.into_iter().find(|language| {
            language.locale().locale_patterns.iter().any(|pattern| {
                normalized == *pattern
                    || normalized
                        .strip_prefix(pattern)
                        .is_some_and(|suffix| suffix.starts_with('-'))
            })
        })
    }

    pub(crate) fn index(self) -> usize {
        self.0
    }

    pub(crate) fn from_index(index: usize) -> Option<Self> {
        (index < generated::LANGUAGE_COUNT).then_some(Self(index))
    }

    pub(crate) fn windows_font(self) -> Option<(&'static str, &'static str)> {
        self.locale().windows_font
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Strings {
    pub window_title: &'static str,
    pub refresh: &'static str,
    pub update_frequency: &'static str,
    pub one_minute: &'static str,
    pub five_minutes: &'static str,
    pub fifteen_minutes: &'static str,
    pub one_hour: &'static str,
    pub models: &'static str,
    pub claude_code_model: &'static str,
    pub codex_model: &'static str,
    pub antigravity_model: &'static str,
    pub opencode_model: &'static str,
    pub cursor_model: &'static str,
    pub grok_model: &'static str,
    pub copilot_model: &'static str,
    pub settings: &'static str,
    pub start_with_windows: &'static str,
    pub language: &'static str,
    pub system_default: &'static str,
    pub check_for_updates: &'static str,
    pub checking_for_updates: &'static str,
    pub updates: &'static str,
    pub update_in_progress: &'static str,
    pub up_to_date: &'static str,
    pub up_to_date_short: &'static str,
    pub update_failed: &'static str,
    pub applying_update: &'static str,
    pub update_to: &'static str,
    pub update_available: &'static str,
    pub update_prompt_now: &'static str,
    pub exit: &'static str,
    pub session_window: &'static str,
    pub weekly_window: &'static str,
    pub cursor_auto_window: &'static str,
    pub cursor_api_window: &'static str,
    pub now: &'static str,
    pub day_suffix: &'static str,
    pub hour_suffix: &'static str,
    pub minute_suffix: &'static str,
    pub second_suffix: &'static str,
    pub token_expired_title: &'static str,
    pub token_expired_body: &'static str,
    pub codex_token_expired_title: &'static str,
    pub codex_token_expired_body: &'static str,
    pub antigravity_token_expired_title: &'static str,
    pub antigravity_token_expired_body: &'static str,
    pub opencode_token_expired_title: &'static str,
    pub opencode_token_expired_body: &'static str,
    pub cursor_token_expired_title: &'static str,
    pub cursor_token_expired_body: &'static str,
    pub grok_token_expired_title: &'static str,
    pub grok_token_expired_body: &'static str,
    pub copilot_token_expired_title: &'static str,
    pub copilot_token_expired_body: &'static str,
    pub codex_window_title: &'static str,
    pub antigravity_window_title: &'static str,
    pub opencode_window_title: &'static str,
    pub cursor_window_title: &'static str,
    pub grok_window_title: &'static str,
    pub copilot_window_title: &'static str,
}

pub fn resolve_language(language_override: Option<LanguageId>) -> LanguageId {
    language_override.unwrap_or_else(detect_system_language)
}

pub fn detect_system_language() -> LanguageId {
    preferred_ui_languages()
        .into_iter()
        .find_map(|locale| LanguageId::from_code(&locale))
        .or_else(default_ui_locale)
        .or_else(default_locale_name)
        .unwrap_or(LanguageId::English)
}

pub fn update_via_winget(language: LanguageId) -> &'static str {
    language.update_via_winget_label()
}

fn preferred_ui_languages() -> Vec<String> {
    unsafe {
        let mut num_languages = 0u32;
        let mut buffer_len = 0u32;
        if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut num_languages, None, &mut buffer_len)
            .is_err()
            || buffer_len == 0
        {
            return Vec::new();
        }

        let mut buffer = vec![0u16; buffer_len as usize];
        if GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut num_languages,
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut buffer_len,
        )
        .is_err()
        {
            return Vec::new();
        }

        buffer
            .split(|unit| *unit == 0)
            .filter(|part| !part.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }
}

fn default_ui_locale() -> Option<LanguageId> {
    unsafe {
        let lang_id = GetUserDefaultUILanguage();
        let mut buffer = [0u16; MAX_LOCALE_NAME as usize];
        let len = LCIDToLocaleName(
            lang_id as u32,
            Some(&mut buffer),
            LOCALE_ALLOW_NEUTRAL_NAMES,
        );
        if len <= 1 {
            return None;
        }
        let locale = String::from_utf16_lossy(&buffer[..(len as usize - 1)]);
        LanguageId::from_code(&locale)
    }
}

fn default_locale_name() -> Option<LanguageId> {
    unsafe {
        let mut buffer = [0u16; MAX_LOCALE_NAME as usize];
        let len = GetUserDefaultLocaleName(&mut buffer);
        if len <= 1 {
            return None;
        }
        let locale = String::from_utf16_lossy(&buffer[..(len as usize - 1)]);
        LanguageId::from_code(&locale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_non_english_locale_translates_the_primary_dashboard_workflow() {
        let keys = [
            "Open Dashboard",
            "Theme Studio",
            "Assets",
            "Every 5 minutes",
            "Enabled",
            "Active theme",
            "Save changes?",
            "Save and continue",
            "Discard changes",
            "Cancel",
            "New theme",
            "Create",
            "Duplicate theme",
            "Create copy",
            "Delete theme?",
            "Delete theme",
            "Delete asset?",
            "Delete",
            "Scene",
            "Add layer",
            "Background",
            "Content type",
            "Apply",
            "Import...",
            "Export...",
            "Import a theme or package",
            "Export theme package",
            "Unable to import theme",
            "Unable to export theme package",
            "Save or discard changes before importing",
            "Add images once, reuse them across themes, or drop image files here to import them.",
            "Theme Studio packages and themes",
            "Theme packages",
            "Theme files",
            "All files",
            "Theme Studio packages",
            "Images",
            "Builder",
            "Build safe mouse actions that affect layers at runtime.",
            "Choose one action for this context menu item.",
            "Enter actions...",
            "Show dashboard",
            "Toggle dashboard",
            "Context Menus",
            "Show context menu",
            "Set property",
            "Reset property",
            "Increase value",
            "Decrease value",
            "Run layer actions",
            "Show widget",
            "Check for updates",
            "Name the new theme",
            "Theme name",
            "Name the editable copy",
            "Are you sure you want to delete {name}?",
            "Delete context menu?",
            "Delete context menu",
            "Are you sure you want to delete {name} from the asset library and all themes using it?",
        ];

        for language in LanguageId::ALL
            .into_iter()
            .filter(|language| *language != LanguageId::English)
        {
            for key in keys {
                assert_ne!(
                    language.text(key),
                    key,
                    "{} is missing the essential translation for {key:?}",
                    language.code()
                );
            }
        }
    }

    #[test]
    fn untranslated_specialist_text_falls_back_to_english() {
        let japanese = LanguageId::from_code("ja").unwrap();
        assert_eq!(
            japanese.text("A future specialist label"),
            "A future specialist label"
        );
    }

    #[test]
    fn supported_locale_codes_are_recognized() {
        for (index, language) in LanguageId::ALL.into_iter().enumerate() {
            assert_eq!(LanguageId::from_index(index), Some(language));
            assert_eq!(
                LanguageId::from_code(language.code()),
                Some(language),
                "{}",
                language.code()
            );
        }

        assert_eq!(LanguageId::from_code("tr_TR").unwrap().code(), "tr");
        assert_eq!(LanguageId::from_code("zh-HK").unwrap().code(), "zh-TW");
        assert_eq!(LanguageId::from_code("zh-SG").unwrap().code(), "zh-CN");
    }

    #[test]
    fn generated_translation_catalogues_are_sorted_and_complete() {
        let expected_keys = LanguageId::English.locale().translations.len();
        assert!(expected_keys > 0);

        for language in LanguageId::ALL {
            let translations = language.locale().translations;
            assert_eq!(translations.len(), expected_keys, "{}", language.code());
            assert!(
                translations.windows(2).all(|pair| pair[0].0 < pair[1].0),
                "{} translations are not sorted",
                language.code()
            );
            assert!(translations
                .iter()
                .all(|(_, value)| !value.trim().is_empty()));
        }
    }

    #[test]
    fn every_source_text_key_is_catalogued_and_translated_in_ukrainian() {
        const UK_IDENTICAL_ALLOWLIST: &[&str] = &[
            "Antigravity",
            "Claude Code",
            "Codex",
            "Copilot",
            "Cursor",
            "Grok",
            "OpenCode",
            "URL",
            "X",
            "Y",
        ];

        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rust_files(&src_dir, &mut files);
        files.sort();
        assert!(!files.is_empty(), "No Rust source files found under src/");

        let mut source_keys = std::collections::BTreeSet::new();
        for file in &files {
            let content = std::fs::read_to_string(file)
                .unwrap_or_else(|error| panic!("Failed to read {}: {error}", file.display()));
            let is_loc_mod = file.ends_with("mod.rs")
                && file
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .is_some_and(|name| name == "localization");
            for key in extract_text_literal_keys(&content) {
                if is_loc_mod && key == "A future specialist label" {
                    continue;
                }
                source_keys.insert(key);
            }
        }
        assert!(!source_keys.is_empty(), "No .text(\"...\") keys discovered");

        let english_translations = LanguageId::English.locale().translations;
        let uk_translations = LanguageId::from_code("uk")
            .expect("uk locale must exist")
            .locale()
            .translations;

        for key in &source_keys {
            assert!(
                english_translations
                    .binary_search_by(|(entry, _)| (*entry).cmp(key.as_str()))
                    .is_ok(),
                "Key {key:?} used in src/ is missing from en.toml and locale catalogues"
            );
            let uk_idx = uk_translations
                .binary_search_by(|(entry, _)| (*entry).cmp(key.as_str()))
                .unwrap_or_else(|_| {
                    panic!("Key {key:?} used in src/ is missing from uk.toml");
                });
            let uk_value = uk_translations[uk_idx].1;
            if !UK_IDENTICAL_ALLOWLIST.contains(&key.as_str()) {
                assert_ne!(
                    uk_value,
                    key.as_str(),
                    "Key {key:?} used in src/ is missing a Ukrainian translation in uk.toml"
                );
            }
        }

        for &(key, value) in uk_translations {
            if !UK_IDENTICAL_ALLOWLIST.contains(&key) {
                assert_ne!(
                    value, key,
                    "uk.toml entry {key:?} still uses the untranslated English key"
                );
            }
        }
    }

    fn collect_rust_files(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("Failed to read {}: {error}", dir.display()));
        for entry in entries {
            let path = entry.expect("Failed to read dir entry").path();
            if path.is_dir() {
                collect_rust_files(&path, files);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum ScanToken {
        Ident(String),
        Str(String),
        Punct(char),
        Other,
    }

    fn extract_text_literal_keys(source: &str) -> Vec<String> {
        let tokens = tokenize_rust_source(source);
        let mut keys = Vec::new();
        for idx in 0..tokens.len().saturating_sub(4) {
            if tokens[idx] == ScanToken::Punct('.')
                && matches!(&tokens[idx + 1], ScanToken::Ident(name) if name == "text")
                && tokens[idx + 2] == ScanToken::Punct('(')
            {
                if let ScanToken::Str(value) = &tokens[idx + 3] {
                    let mut next = idx + 4;
                    if tokens.get(next) == Some(&ScanToken::Punct(',')) {
                        next += 1;
                    }
                    if tokens.get(next) == Some(&ScanToken::Punct(')')) {
                        keys.push(value.clone());
                    }
                }
            }
        }
        keys
    }

    fn tokenize_rust_source(source: &str) -> Vec<ScanToken> {
        let bytes = source.as_bytes();
        let mut tokens = Vec::new();
        let mut i = 0;

        while i < bytes.len() {
            let byte = bytes[i];
            if byte.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            if bytes[i..].starts_with(b"//") {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if bytes[i..].starts_with(b"/*") {
                i += 2;
                let mut depth = 1usize;
                while i < bytes.len() && depth > 0 {
                    if bytes[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if bytes[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            if byte == b'"' {
                let (parsed, next_i) = parse_regular_string(source, i + 1);
                tokens.push(ScanToken::Str(parsed));
                i = next_i;
                continue;
            }
            if byte == b'r' {
                if let Some((parsed, next_i)) = try_parse_raw_string(source, i) {
                    tokens.push(ScanToken::Str(parsed));
                    i = next_i;
                    continue;
                }
            }
            if byte == b'\'' {
                i = skip_char_or_lifetime(source, i);
                tokens.push(ScanToken::Other);
                continue;
            }
            if byte.is_ascii_alphabetic() || byte == b'_' {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                tokens.push(ScanToken::Ident(source[start..i].to_string()));
                continue;
            }
            if matches!(byte, b'.' | b'(' | b')' | b',') {
                tokens.push(ScanToken::Punct(byte as char));
            } else {
                tokens.push(ScanToken::Other);
            }
            i += 1;
        }

        tokens
    }

    fn parse_regular_string(source: &str, mut i: usize) -> (String, usize) {
        let mut out = String::new();
        while i < source.len() {
            let rest = &source[i..];
            let Some(ch) = rest.chars().next() else {
                break;
            };
            i += ch.len_utf8();
            if ch == '"' {
                return (out, i);
            }
            if ch != '\\' {
                out.push(ch);
                continue;
            }
            let Some(esc) = source[i..].chars().next() else {
                break;
            };
            i += esc.len_utf8();
            match esc {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '\'' => out.push('\''),
                '0' => out.push('\0'),
                '\r' => {
                    if source[i..].starts_with('\n') {
                        i += 1;
                    }
                    while i < source.len() && matches!(source.as_bytes()[i], b' ' | b'\t') {
                        i += 1;
                    }
                }
                '\n' => {
                    while i < source.len() && matches!(source.as_bytes()[i], b' ' | b'\t') {
                        i += 1;
                    }
                }
                'x' if i + 2 <= source.len() => {
                    if let Ok(val) = u8::from_str_radix(&source[i..i + 2], 16) {
                        out.push(val as char);
                        i += 2;
                    }
                }
                'u' if source[i..].starts_with('{') => {
                    if let Some(close) = source[i + 1..].find('}') {
                        let hex = &source[i + 1..i + 1 + close];
                        if let Some(decoded) =
                            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                        {
                            out.push(decoded);
                        }
                        i += 1 + close + 1;
                    }
                }
                other => out.push(other),
            }
        }
        (out, i)
    }

    fn try_parse_raw_string(source: &str, start: usize) -> Option<(String, usize)> {
        let bytes = source.as_bytes();
        let mut j = start + 1;
        let mut hashes = 0usize;
        while j < bytes.len() && bytes[j] == b'#' {
            hashes += 1;
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'"' {
            return None;
        }
        let content_start = j + 1;
        let end_delim = format!("\"{}", "#".repeat(hashes));
        let end_rel = source[content_start..].find(&end_delim)?;
        let content = source[content_start..content_start + end_rel].to_string();
        Some((content, content_start + end_rel + end_delim.len()))
    }

    fn skip_char_or_lifetime(source: &str, start: usize) -> usize {
        let bytes = source.as_bytes();
        let mut i = start + 1;
        if i >= bytes.len() {
            return i;
        }
        if bytes[i] == b'\\' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'\'' {
                i += 1;
            }
            return (i + 1).min(bytes.len());
        }
        if let Some(ch) = source[i..].chars().next() {
            let after_ch = i + ch.len_utf8();
            if after_ch < bytes.len() && bytes[after_ch] == b'\'' {
                return after_ch + 1;
            }
        }
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        i
    }
}
