use super::{ConfigResult, Diagnostic, Origin};

pub fn validate_relative_path(path: &str, origin: &Origin) -> ConfigResult<()> {
    if path.contains(['\\', ':', '<', '>', '"', '|', '?', '*'])
        || path.chars().any(char::is_control)
        || path.split('/').any(|part| {
            matches!(part, "" | "." | "..")
                || part.ends_with(['.', ' '])
                || device(part)
                || short_alias(part)
        })
    {
        return Err(Diagnostic::new(
            "invalid_path",
            origin,
            "unsafe relative path component",
        ));
    }
    Ok(())
}

fn short_alias(part: &str) -> bool {
    let base = part.split('.').next().unwrap_or("");
    base.len() <= 8
        && base
            .rsplit_once('~')
            .is_some_and(|(_, tail)| !tail.is_empty() && tail.bytes().all(|c| c.is_ascii_digit()))
}

fn device(part: &str) -> bool {
    let base = part
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_uppercase();
    matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        base.strip_prefix(prefix).is_some_and(|tail| {
            matches!(
                tail,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

pub(crate) fn legacy_path(path: &str, origin: &Origin) -> ConfigResult<()> {
    if path.contains(['\\', ':'])
        || path.chars().any(char::is_control)
        || path.split('/').any(|p| matches!(p, "" | "." | ".."))
    {
        return Err(Diagnostic::new(
            "invalid_path",
            origin,
            "invalid legacy protected path",
        ));
    }
    Ok(())
}

/// v0.1-compatible Unicode lowercase plus exact/slash-boundary matching.
pub fn protected_matches(path: &str, prefix: &str) -> bool {
    let path = path.to_lowercase();
    let prefix = prefix.to_lowercase();
    path == prefix
        || path
            .strip_prefix(&prefix)
            .is_some_and(|tail| tail.starts_with('/'))
}
