//! Physical path planning for file-backed script destinations.

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};

use crate::snapshot_middleware::Middleware;

#[derive(Debug)]
pub(crate) struct PromotionPaths {
    pub directory: PathBuf,
    pub originals: Vec<(PathBuf, PathBuf)>,
}

/// Preserve the physical stem, script kind, and actual extension. A metadata Name
/// override must not silently rename the parent as a side effect of receiving a child.
pub(crate) fn plan_promotion(
    source: &Path,
    middleware: Middleware,
) -> anyhow::Result<PromotionPaths> {
    let kind = match middleware {
        Middleware::ModuleScript => "",
        Middleware::ServerScript | Middleware::RunContextServerScript => ".server",
        Middleware::ClientScript | Middleware::RunContextClientScript => ".client",
        Middleware::LegacyServerScript => ".legacy",
        Middleware::LegacyClientScript => ".local",
        Middleware::PluginScript => ".plugin",
        _ => anyhow::bail!("The destination is not a promotable file-backed script"),
    };
    let filename = source
        .file_name()
        .and_then(|name| name.to_str())
        .context("The destination filename is not valid Unicode")?;
    let extension = if filename.ends_with(".luau") {
        ".luau"
    } else {
        ".lua"
    };
    let suffix = format!("{kind}{extension}");
    let stem = filename
        .strip_suffix(&suffix)
        .context("Unsupported promotion filename")?;
    ensure!(
        !stem.is_empty() && stem != "." && stem != "..",
        "Invalid promotion stem"
    );
    // A plain module with a reserved dotted suffix would select another middleware.
    let mapped = crate::snapshot_middleware::default_sync_rules()
        .iter()
        .find(|rule| rule.matches(source))
        .context("The destination has no default mapping")?;
    ensure!(
        super::file_names::script_kind_suffix(mapped.middleware)
            == super::file_names::script_kind_suffix(middleware),
        "Custom script mappings cannot be promoted"
    );
    let parent = source
        .parent()
        .context("The destination source has no parent")?;
    let directory = parent.join(stem);
    let mut originals = vec![(
        source.to_path_buf(),
        directory.join(format!("init{suffix}")),
    )];
    for extension in ["json", "jsonc"] {
        originals.push((
            parent.join(format!("{stem}.meta.{extension}")),
            directory.join(format!("init.meta.{extension}")),
        ));
    }
    Ok(PromotionPaths {
        directory,
        originals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_initializer_kind_and_actual_extension() {
        for (kind, middleware) in [
            ("", Middleware::ModuleScript),
            (".server", Middleware::ServerScript),
            (".client", Middleware::ClientScript),
            (".server", Middleware::RunContextServerScript),
            (".client", Middleware::RunContextClientScript),
            (".legacy", Middleware::LegacyServerScript),
            (".local", Middleware::LegacyClientScript),
            (".plugin", Middleware::PluginScript),
        ] {
            for extension in ["lua", "luau"] {
                let source = PathBuf::from(format!("/src/Physical.extra{kind}.{extension}"));
                let plan = plan_promotion(&source, middleware).unwrap();
                assert_eq!(plan.directory, Path::new("/src/Physical.extra"));
                assert_eq!(
                    plan.originals[0].1,
                    plan.directory.join(format!("init{kind}.{extension}"))
                );
                assert_eq!(
                    plan.originals[1].0,
                    Path::new("/src/Physical.extra.meta.json")
                );
                assert_eq!(plan.originals[2].1, plan.directory.join("init.meta.jsonc"));
            }
        }
    }
}
