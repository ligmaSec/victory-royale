//! Identify the active Proton installation from inside the Wine process.
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

fn installation(data: &Path) -> Option<&Path> {
    // Wine sets WINEDATADIR from the running Wine binary. Both current Proton
    // (files) and older Proton (dist) keep their version beside the proton script.
    let root = data.ancestors().nth(3)?;
    let layout = data.strip_prefix(root).ok()?;
    if layout != Path::new("files/share/wine") && layout != Path::new("dist/share/wine") {
        return None;
    }
    root.join("proton").is_file().then_some(root)
}

// UMU sets both variables after resolving its requested alias. A tool may
// share another installation's Wine files and still have its own build ID.
fn umu_tool<'a>(proton: &'a str, tools: &str) -> Option<&'a str> {
    (proton.starts_with('/') && tools.split(':').next() == Some(proton)).then_some(proton)
}

fn build_id(root: &Path) -> Option<String> {
    let path = root.join("version");
    if !path.is_file() {
        return None;
    }
    let mut line = String::new();
    let len = BufReader::new(File::open(path).ok()?.take(256))
        .read_line(&mut line)
        .ok()?;
    let value = line.trim_end_matches(['\n', '\r']);
    // Print only a complete, printable build label, never an arbitrary path.
    (len < 256
        && value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" .+_-()".contains(&b)))
    .then(|| value.to_owned())
}

#[cfg(windows)]
pub(super) fn version() -> Option<String> {
    use std::path::PathBuf;

    let data = std::env::var_os("WINEDATADIR")?;
    if let (Ok(proton), Ok(tools)) = (
        std::env::var("PROTONPATH"),
        std::env::var("STEAM_COMPAT_TOOL_PATHS"),
    ) && let Some(tool) = umu_tool(&proton, &tools)
    {
        // Wine exposes Linux paths through the unix device even without Z:.
        let root = PathBuf::from(format!(r"\\?\unix{}", tool.replace('/', r"\")));
        if root.join("proton").is_file() {
            return Some(build_id(&root).unwrap_or_else(|| "unavailable".to_owned()));
        }
    }
    // Wine gives Windows processes an NT path, e.g. \??\X:\Proton\files\share\wine.
    let data = data.to_str()?;
    let path = match data.strip_prefix(r"\??\") {
        Some(rest) => PathBuf::from(format!(r"\\?\{rest}")),
        None => PathBuf::from(data),
    };
    let root = installation(&path)?;
    Some(build_id(root).unwrap_or_else(|| "unavailable".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn unresolved_or_stale_alias_does_not_override_wine() {
        assert_eq!(
            umu_tool("/runtime A", "/runtime A:/steamrt"),
            Some("/runtime A")
        );
        assert_eq!(umu_tool("GE-Proton", "/tools/GE-Proton:/steamrt"), None);
        assert_eq!(umu_tool("/stale", "/active:/stale"), None);
    }

    #[test]
    fn running_wine_tree_selects_its_own_proton_build() {
        let tmp = tempfile::tempdir().unwrap();
        for (dist, label) in [
            ("files", "1795031687 UMU-Proton-11.0-2e"),
            ("dist", "GE-Proton10-27"),
        ] {
            let root = tmp.path().join(label);
            let wine = root.join(dist).join("share/wine");
            fs::create_dir_all(&wine).unwrap();
            fs::write(
                root.join("version"),
                format!("{label}\r\nunrelated metadata\n"),
            )
            .unwrap();
            assert!(installation(&wine).is_none());
            fs::write(root.join("proton"), "").unwrap();
            assert_eq!(
                build_id(installation(&wine).unwrap()).as_deref(),
                Some(label)
            );
            // Ordinary system Wine must not pick up an unrelated version file.
            assert!(installation(&root.join("usr/share/wine")).is_none());
            for bad in [
                "",
                "  ",
                "\x1b[31mprivate",
                "/private/tool/path",
                &"a".repeat(256),
            ] {
                fs::write(root.join("version"), bad).unwrap();
                assert!(build_id(&root).is_none());
            }
        }
    }
}
