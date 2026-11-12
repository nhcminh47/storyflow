//! Host authorization lives outside documents and project checkouts.
use super::{
    ConfigResult, Diagnostic, Origin, discovery,
    external::{self, TrustedRoot},
};
use std::path::Path;

pub const MAX_HOST_CONTEXT_BYTES: usize = 65_536;

/// The invoking user supplies both an absolute manifest path and a separately
/// authorized absolute directory. Neither value may come from the manifest.
/// Keep the opened target live through location validation and bounded reading.
pub fn read_host_context(path: &Path, authorized_root: &Path) -> ConfigResult<Vec<u8>> {
    let at = Origin::host("host_context");
    external::absolute(path, &at)?;
    external::absolute(authorized_root, &at)?;
    let relative = path
        .strip_prefix(authorized_root)
        .ok()
        .and_then(Path::to_str)
        .ok_or_else(|| {
            Diagnostic::new(
                "unauthorized_host_context",
                &at,
                "host context must be inside the separately authorized directory",
            )
        })?
        .replace('\\', "/");
    let root = TrustedRoot::establish(authorized_root, &at)?;
    outside_checkout(&root.path, &at)?;
    root.read_checked_bounded(&relative, false, &at, MAX_HOST_CONTEXT_BYTES, |opened| {
        outside_checkout(
            opened.parent().ok_or_else(|| {
                Diagnostic::new(
                    "untrusted_host_context",
                    &at,
                    "opened manifest has no parent",
                )
            })?,
            &at,
        )
    })?
    .ok_or_else(|| Diagnostic::new("missing_host_context", &at, "host context is unavailable"))
}

fn outside_checkout(path: &Path, at: &Origin) -> ConfigResult<()> {
    discovery::reject_checkout(path, at).map_err(|_| {
        Diagnostic::new(
            "untrusted_host_context",
            at,
            "host context authorization and opened manifest must be outside every Git checkout",
        )
    })
}
