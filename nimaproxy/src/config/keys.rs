//! API key configuration.

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug)]
pub struct KeyEntry {
    pub key: String,
    pub label: Option<String>,
    /// Parked key: stays in the config for reference but is never leased.
    /// Previously this field was silently ignored by serde, so a parked key
    /// still entered the rotation and was discovered broken per-request.
    #[serde(default)]
    pub disabled: bool,
}

/// Extract the variable name when `value` is exactly `${IDENT}` with
/// `IDENT` matching `[A-Za-z_][A-Za-z0-9_]*`. Anything else (embedded
/// placeholders like `abc${X}def`, malformed names like `${1BAD}`, bare
/// `$X`) is a literal key and returns `None`.
fn placeholder_var(value: &str) -> Option<&str> {
    let inner = value.strip_prefix("${")?.strip_suffix("}")?;
    let mut chars = inner.chars();
    match chars.next()? {
        'A'..='Z' | 'a'..='z' | '_' => {}
        _ => return None,
    }
    if !chars.all(|c| matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_')) {
        return None;
    }
    Some(inner)
}

/// Replace `${VAR}` key placeholders with environment values, in place.
///
/// Each entry resolves independently; one unset (or set-but-empty)
/// variable fails the whole load (fail closed — an empty key would be
/// silently leased and fail per-request). The error names the variable and
/// the entry but never echoes any key material.
pub fn resolve_key_env(keys: &mut [KeyEntry]) -> Result<(), String> {
    for (index, entry) in keys.iter_mut().enumerate() {
        let Some(var) = placeholder_var(&entry.key) else {
            continue;
        };
        match std::env::var(var) {
            Ok(value) if !value.is_empty() => entry.key = value,
            Ok(_) => {
                let who = entry_who(index, entry.label.as_deref());
                return Err(format!(
                    "{who} references environment variable '{var}' which is empty"
                ));
            }
            Err(_) => {
                let who = entry_who(index, entry.label.as_deref());
                return Err(format!(
                    "{who} references unset environment variable '{var}'"
                ));
            }
        }
    }
    Ok(())
}

fn entry_who(index: usize, label: Option<&str>) -> String {
    match label {
        Some(label) => format!("key entry {index} (label '{label}')"),
        None => format!("key entry {index}"),
    }
}
