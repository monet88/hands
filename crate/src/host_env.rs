use std::ffi::{OsStr, OsString};

pub(crate) const REG_SZ: u32 = 1;
pub(crate) const REG_EXPAND_SZ: u32 = 2;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegistryPathError {
    UnsupportedType(u32),
    OddByteLength(usize),
    MissingNullTerminator,
    InvalidUtf16,
    ExpansionFailed,
}

pub(crate) fn parse_registry_path_payload<F>(
    data_type: u32,
    data: &[u8],
    expand: F,
) -> Result<String, RegistryPathError>
where
    F: FnOnce(&str) -> Option<String>,
{
    if data_type != REG_SZ && data_type != REG_EXPAND_SZ {
        return Err(RegistryPathError::UnsupportedType(data_type));
    }
    if data.len() % 2 != 0 {
        return Err(RegistryPathError::OddByteLength(data.len()));
    }

    let units = data
        .chunks_exact(2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect::<Vec<_>>();
    if units.last().copied() != Some(0) {
        return Err(RegistryPathError::MissingNullTerminator);
    }

    let value = String::from_utf16(&units[..units.len() - 1])
        .map_err(|_| RegistryPathError::InvalidUtf16)?;
    if data_type == REG_EXPAND_SZ {
        expand(&value).ok_or(RegistryPathError::ExpansionFailed)
    } else {
        Ok(value)
    }
}

pub(crate) fn merge_path(
    current_path: &OsStr,
    user_path: &OsStr,
) -> Result<OsString, std::env::JoinPathsError> {
    fn key(path: &std::path::Path) -> String {
        path.as_os_str().to_string_lossy().to_ascii_lowercase()
    }

    let mut merged = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in std::env::split_paths(current_path).chain(std::env::split_paths(user_path)) {
        if entry.as_os_str().is_empty() {
            continue;
        }
        if seen.insert(key(&entry)) {
            merged.push(entry);
        }
    }
    std::env::join_paths(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn utf16le_payload(value: &str, terminated: bool) -> Vec<u8> {
        let mut units: Vec<u16> = value.encode_utf16().collect();
        if terminated {
            units.push(0);
        }
        units
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    }

    #[test]
    fn parses_reg_sz_without_expanding_environment_variables() {
        let payload = utf16le_payload(r"%HANDS_HOME%\bin", true);
        let parsed = parse_registry_path_payload(REG_SZ, &payload, |_| {
            panic!("REG_SZ must not invoke environment expansion")
        })
        .expect("valid REG_SZ should parse");

        assert_eq!(parsed, r"%HANDS_HOME%\bin");
    }

    #[test]
    fn parses_and_expands_reg_expand_sz() {
        let payload = utf16le_payload(r"%HANDS_HOME%\bin", true);
        let parsed = parse_registry_path_payload(REG_EXPAND_SZ, &payload, |raw| {
            Some(raw.replace("%HANDS_HOME%", r"C:\Users\test"))
        })
        .expect("valid REG_EXPAND_SZ should parse and expand");

        assert_eq!(parsed, r"C:\Users\test\bin");
    }

    #[test]
    fn rejects_wrong_registry_type() {
        let payload = utf16le_payload(r"C:\Tools", true);
        assert_eq!(
            parse_registry_path_payload(7, &payload, |_| None),
            Err(RegistryPathError::UnsupportedType(7))
        );
    }

    #[test]
    fn rejects_odd_byte_length() {
        let mut payload = utf16le_payload(r"C:\Tools", true);
        payload.pop();
        assert_eq!(
            parse_registry_path_payload(REG_SZ, &payload, |_| None),
            Err(RegistryPathError::OddByteLength(payload.len()))
        );
    }

    #[test]
    fn rejects_non_null_terminated_payload() {
        let payload = utf16le_payload(r"C:\Tools", false);
        assert_eq!(
            parse_registry_path_payload(REG_SZ, &payload, |_| None),
            Err(RegistryPathError::MissingNullTerminator)
        );
    }

    #[test]
    fn reports_expand_failure_only_for_expandable_values() {
        let payload = utf16le_payload(r"%MISSING%\bin", true);
        assert_eq!(
            parse_registry_path_payload(REG_EXPAND_SZ, &payload, |_| None),
            Err(RegistryPathError::ExpansionFailed)
        );
    }

    #[test]
    fn rejects_invalid_utf16() {
        let payload = [0x00, 0xD8, 0x00, 0x00];
        assert_eq!(
            parse_registry_path_payload(REG_SZ, &payload, |_| None),
            Err(RegistryPathError::InvalidUtf16)
        );
    }

    #[test]
    #[cfg(windows)]
    fn merge_skips_empty_entries_preserves_precedence_and_deduplicates() {
        let current = OsString::from(r"C:\current;;C:\shared;C:\current");
        let user = OsString::from(r";C:\shared;C:\user;;C:\user");
        let merged = merge_path(&current, &user).expect("PATH entries should join");
        let entries: Vec<PathBuf> = std::env::split_paths(&merged).collect();

        assert_eq!(
            entries,
            vec![
                PathBuf::from(r"C:\current"),
                PathBuf::from(r"C:\shared"),
                PathBuf::from(r"C:\user"),
            ]
        );
    }

    #[test]
    #[cfg(windows)]
    fn merge_is_idempotent() {
        let current = OsString::from(r"C:\current;C:\shared");
        let user = OsString::from(r"C:\shared;C:\user");
        let once = merge_path(&current, &user).expect("first PATH merge should work");
        let twice = merge_path(&once, &user).expect("second PATH merge should work");

        assert_eq!(once, twice);
    }
}
