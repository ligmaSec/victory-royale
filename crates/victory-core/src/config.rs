//! Where the user's files live.
//!
//! Resolution order for each value: environment variable, then the config
//! file (`VICTORY_CONFIG`, else `./victory.toml`), then the default.
//! A variable that is set but empty counts as unset. Relative paths in the
//! config file are relative to the file, not to the working directory. A
//! leading `~` in a path (from the file or a variable) is the home directory.
//! See `victory.example.toml` at the repository root.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::policy::ensure_outside_repo;

#[derive(Debug, Default, Clone)]
pub struct Config {
    /// The user's dump of the title.
    pub game_dir: Option<PathBuf>,
    /// Root for all derived data. Always outside the repository.
    pub cache_dir: PathBuf,
    /// What titles write and must keep (persistent local storage). Never
    /// inside the cache, which may be deleted at any time.
    pub data_dir: PathBuf,
    /// Other titles used to exercise platform-level code.
    pub test_titles: Vec<PathBuf>,
    /// The config file that was read, if any.
    pub source: Option<PathBuf>,
    /// Where each value above came from.
    pub origins: Origins,
}

/// Where a configured value came from, so `victory config` and errors can
/// name the setting to change instead of leaving the user to guess which of
/// the variable, the file and the default won.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Not set anywhere: the built-in default (or nothing, for `game_dir`).
    #[default]
    Default,
    /// The `[paths]` table of the config file.
    File,
    /// The named environment variable.
    Env(&'static str),
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Origin::Default => f.write_str("default"),
            Origin::File => f.write_str("config file"),
            Origin::Env(name) => f.write_str(name),
        }
    }
}

/// The [`Origin`] of each [`Config`] value.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Origins {
    pub game_dir: Origin,
    pub cache_dir: Origin,
    pub data_dir: Origin,
    pub test_titles: Origin,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    paths: FilePaths,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePaths {
    game_dir: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    #[serde(default)]
    test_titles: Vec<PathBuf>,
}

impl FileConfig {
    /// The `[paths]` table with every relative path made relative to `dir`,
    /// the directory holding the config file. With `VICTORY_CONFIG` pointing
    /// elsewhere, `cache_dir = "cache"` then means the folder next to that
    /// file, not one under whatever directory the tool was started from
    /// (often the repository, which the asset policy refuses). A path that
    /// starts with `~` is under `home` instead, when there is one. A Windows
    /// drive path on another host is refused (see [`ensure_native`]) rather
    /// than anchored here as a relative path, and so is a path holding a
    /// control character (see [`ensure_no_control`]).
    ///
    /// An empty value (`game_dir = ""`, a blank a template leaves to fill
    /// in, or `test_titles = [""]`) counts as unset, like an empty variable.
    /// Anchored, it would be the config file's own folder (often the
    /// repository), or with `./victory.toml` the empty path, which reads
    /// and writes the working directory.
    fn anchored_at(self, dir: &Path, home: Option<&Path>) -> Result<FilePaths> {
        let anchor = |key: &str, p: PathBuf| -> Result<PathBuf> {
            check_setting(&format!("[paths].{key}"), &p)?;
            let p = expand_home(p, home);
            Ok(if p.is_relative() { dir.join(p) } else { p })
        };
        let set = |p: &PathBuf| !p.as_os_str().is_empty();
        let paths = self.paths;
        Ok(FilePaths {
            game_dir: paths
                .game_dir
                .filter(set)
                .map(|p| anchor("game_dir", p))
                .transpose()?,
            cache_dir: paths
                .cache_dir
                .filter(set)
                .map(|p| anchor("cache_dir", p))
                .transpose()?,
            data_dir: paths
                .data_dir
                .filter(set)
                .map(|p| anchor("data_dir", p))
                .transpose()?,
            test_titles: paths
                .test_titles
                .into_iter()
                .filter(set)
                .map(|p| anchor("test_titles", p))
                .collect::<Result<_>>()?,
        })
    }
}

impl Config {
    /// Loads configuration from the environment and the config file.
    pub fn load() -> Result<Self> {
        let env = |k: &str| std::env::var_os(k);
        let home = home_dir(env);
        let file = match config_file(explicit_config(env, home.as_deref())?)? {
            Some(path) => {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                Some((path, text))
            }
            None => None,
        };
        let (source, parsed) = match file {
            Some((path, text)) => {
                let parsed =
                    parse_file(&text).with_context(|| format!("parsing {}", path.display()))?;
                let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
                let paths = parsed
                    .anchored_at(&dir, home.as_deref())
                    .with_context(|| format!("in {}", path.display()))?;
                (Some(path), paths)
            }
            None => (None, FilePaths::default()),
        };
        Self::resolve(parsed, source, env)
    }

    fn resolve(
        file: FilePaths,
        source: Option<PathBuf>,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Result<Self> {
        // A quoted `VICTORY_GAME_DIR="~/games"` reaches us unexpanded.
        let home = home_dir(&env);
        let expand = |p: PathBuf| expand_home(p, home.as_deref());
        // The variable, else the file, with where the value came from.
        let pick = |k: &'static str, from_file: Option<PathBuf>| -> Result<_> {
            Ok(match env(k).filter(|v| !v.is_empty()) {
                Some(v) => {
                    let p = PathBuf::from(v);
                    check_setting(k, &p)?;
                    (Some(expand(p)), Origin::Env(k))
                }
                None if from_file.is_some() => (from_file, Origin::File),
                None => (None, Origin::Default),
            })
        };
        let (test_titles, test_titles_origin) =
            match env("VICTORY_TEST_TITLES").filter(|v| !v.is_empty()) {
                Some(v) => {
                    // Checked before splitting: on this host `:` would cut
                    // `D:\a` in two.
                    check_setting("VICTORY_TEST_TITLES", Path::new(&v))?;
                    let titles: Vec<PathBuf> = std::env::split_paths(&v)
                        .filter(|p| !p.as_os_str().is_empty())
                        .collect();
                    // `/a: /b` passes the check above as a whole.
                    for title in &titles {
                        ensure_trimmed("VICTORY_TEST_TITLES", title)?;
                    }
                    (
                        titles.into_iter().map(expand).collect(),
                        Origin::Env("VICTORY_TEST_TITLES"),
                    )
                }
                None if file.test_titles.is_empty() => (file.test_titles, Origin::Default),
                None => (file.test_titles, Origin::File),
            };
        let (game_dir, game_origin) = pick("VICTORY_GAME_DIR", file.game_dir)?;
        let (cache_dir, cache_origin) = pick("VICTORY_CACHE_DIR", file.cache_dir)?;
        let cache_dir = cache_dir.map_or_else(|| default_dir(Kind::Cache, &env), Ok)?;
        let (data_dir, data_origin) = pick("VICTORY_DATA_DIR", file.data_dir)?;
        let data_dir = data_dir.map_or_else(|| default_dir(Kind::Data, &env), Ok)?;
        let origins = Origins {
            game_dir: game_origin,
            cache_dir: cache_origin,
            data_dir: data_origin,
            test_titles: test_titles_origin,
        };
        ensure_data_outside_cache(&cache_dir, &data_dir, &origins)?;
        if let Some(game) = &game_dir {
            ensure_game_apart(game, &cache_dir, &data_dir, &origins)?;
        }
        Ok(Config {
            game_dir,
            cache_dir,
            data_dir,
            test_titles,
            source,
            origins,
        })
    }

    /// The game directory, or a helpful error if it is not configured.
    pub fn require_game_dir(&self) -> Result<&Path> {
        self.game_dir.as_deref().context(
            "no game directory configured: set VICTORY_GAME_DIR or [paths].game_dir in \
             victory.toml (see victory.example.toml)",
        )
    }

    /// Returns `<cache>/<sub>`, creating it, after checking the asset policy.
    pub fn cache_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.cache_dir, sub)
    }

    /// Returns `<data>/<sub>`, creating it, after checking the asset policy.
    pub fn data_subdir(&self, sub: &str) -> Result<PathBuf> {
        subdir(&self.data_dir, sub)
    }
}

/// The keys of the `[paths]` table.
const PATH_KEYS: [&str; 4] = ["game_dir", "cache_dir", "data_dir", "test_titles"];

/// Parses a config file. A `[paths]` key written at the top level, above
/// the `[paths]` header or in a file without one (`game_dir = '...'` on its
/// own is an easy first file to write), is otherwise reported only as
/// "unknown field `game_dir`, expected `paths`", which does not say what
/// to change; the error names the key and the table it belongs in.
fn parse_file(text: &str) -> Result<FileConfig> {
    let err = match toml::from_str(text) {
        Ok(parsed) => return Ok(parsed),
        Err(e) => e,
    };
    let misplaced: Vec<&str> = text
        .parse::<toml::Table>()
        .map(|top| {
            PATH_KEYS
                .into_iter()
                .filter(|k| top.contains_key(*k))
                .collect()
        })
        .unwrap_or_default();
    if misplaced.is_empty() {
        return Err(err.into());
    }
    anyhow::bail!(
        "{} must be under the [paths] table: put a `[paths]` line above {} \
         (see victory.example.toml)",
        misplaced.join(", "),
        if misplaced.len() == 1 { "it" } else { "them" }
    );
}

/// `VICTORY_CONFIG`, empty counting as unset, with a leading `~` as the
/// home directory like every other path variable: a quoted
/// `VICTORY_CONFIG="~/victory.toml"` reaches us unexpanded and would
/// otherwise be reported as not existing. Checked like the other path
/// variables (see [`check_setting`]).
fn explicit_config(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    home: Option<&Path>,
) -> Result<Option<PathBuf>> {
    env("VICTORY_CONFIG")
        .filter(|v| !v.is_empty())
        .map(|v| {
            let p = PathBuf::from(v);
            check_setting("VICTORY_CONFIG", &p)?;
            Ok(expand_home(p, home))
        })
        .transpose()
}

/// The config file to read: `explicit` (`VICTORY_CONFIG`), which must be a
/// file, else `./victory.toml` when that is a file.
fn config_file(explicit: Option<PathBuf>) -> Result<Option<PathBuf>> {
    let Some(path) = explicit else {
        let default = PathBuf::from("victory.toml");
        return Ok(default.is_file().then_some(default));
    };
    if path.is_file() {
        return Ok(Some(path));
    }
    if path.is_dir() {
        // An easy slip: the folder holding victory.toml instead of the file.
        let inside = path.join("victory.toml");
        let hint = if inside.is_file() {
            format!("; did you mean {}?", inside.display())
        } else {
            String::new()
        };
        anyhow::bail!(
            "VICTORY_CONFIG points at {}, which is a folder, not a config file{hint}",
            path.display()
        );
    }
    anyhow::bail!(
        "VICTORY_CONFIG points at {}, which does not exist",
        path.display()
    );
}

/// The data folder holds what titles must keep and the cache may be deleted
/// at any time, so data inside (or equal to) the cache would be lost with it.
/// Easy to set up by accident, e.g. `VICTORY_CACHE_DIR=~/victory` with
/// `VICTORY_DATA_DIR=~/victory/data`. Compared without touching the disk
/// (no symlinks followed; the folders need not exist yet), but with `.` and
/// `..` folded away (see [`lexical`]): otherwise `~/.cache/victory/../data`
/// would be refused although it is a sibling of the cache, and a cache of
/// `~/victory/tmp/..` would not be seen to hold `~/victory/data`. The error
/// says where each folder came from, so it is clear which setting caused it.
fn ensure_data_outside_cache(cache_dir: &Path, data_dir: &Path, origins: &Origins) -> Result<()> {
    if is_within(data_dir, cache_dir, FOLD_CASE) {
        if origins.cache_dir == Origin::Default && origins.data_dir == Origin::Default {
            // Both defaults: XDG_CACHE_HOME and XDG_DATA_HOME (or one under
            // the other) are the cause, and the generic advice would not
            // say so.
            anyhow::bail!(
                "the default data folder {} is inside the default cache folder {}, which \
                 may be deleted at any time, because XDG_DATA_HOME is inside (or the same \
                 as) XDG_CACHE_HOME; point them at different folders, or set \
                 VICTORY_DATA_DIR or [paths].data_dir to a folder outside the cache",
                data_dir.display(),
                cache_dir.display()
            );
        }
        anyhow::bail!(
            "the data folder {} (from {}) is inside the cache folder {} (from {}), which may \
             be deleted at any time; set VICTORY_DATA_DIR or [paths].data_dir to a folder \
             outside it",
            data_dir.display(),
            origins.data_dir,
            cache_dir.display(),
            origins.cache_dir
        );
    }
    Ok(())
}

/// The game folder is the user's own dump: Victory reads it and never
/// writes there, and it must survive the cache being cleared. So it may not
/// be inside (or equal to) the cache folder, where clearing the cache would
/// delete it, and neither the cache nor the data folder may be inside it,
/// where derived files and saves would be mixed into the dump. Each is an
/// easy slip with one parent folder for everything, e.g.
/// `VICTORY_CACHE_DIR=~/title` with `VICTORY_GAME_DIR=~/title/game`, or
/// `[paths].cache_dir = "cache"` in a `victory.toml` kept inside the dump.
/// Compared like [`ensure_data_outside_cache`]: lexically, without the disk.
fn ensure_game_apart(game: &Path, cache: &Path, data: &Path, origins: &Origins) -> Result<()> {
    if is_within(game, cache, FOLD_CASE) {
        anyhow::bail!(
            "the game folder {} (from {}) is inside the cache folder {} (from {}), which may \
             be deleted at any time; set VICTORY_CACHE_DIR or [paths].cache_dir to a folder \
             outside the game folder",
            game.display(),
            origins.game_dir,
            cache.display(),
            origins.cache_dir
        );
    }
    for (name, dir, origin, setting) in [
        (
            "cache",
            cache,
            origins.cache_dir,
            "VICTORY_CACHE_DIR or [paths].cache_dir",
        ),
        (
            "data",
            data,
            origins.data_dir,
            "VICTORY_DATA_DIR or [paths].data_dir",
        ),
    ] {
        if is_within(dir, game, FOLD_CASE) {
            anyhow::bail!(
                "the {name} folder {} (from {origin}) is inside the game folder {} (from {}), \
                 which holds your dump and is only read; set {setting} to a folder outside it",
                dir.display(),
                game.display(),
                origins.game_dir
            );
        }
    }
    Ok(())
}

/// Whether folder names differ only by case on the usual file systems of
/// this host: NTFS (Windows) and APFS (macOS) ignore case by default, so
/// there `C:\Victory\cache` holds `c:\victory\cache\data`.
const FOLD_CASE: bool = cfg!(any(windows, target_os = "macos"));

/// Whether `path` is `parent` or inside it, compared like
/// [`ensure_data_outside_cache`] says: lexically (see [`lexical`]), and
/// ignoring ASCII case when `fold_case` (a parameter so both behaviours can
/// be tested on any host). Without folding, a cache of `C:\Victory` and a
/// data folder of `c:\victory\data` would be taken as unrelated on Windows
/// although they are one folder and its child.
fn is_within(path: &Path, parent: &Path, fold_case: bool) -> bool {
    let path = lexical(path);
    let parent = lexical(parent);
    if !fold_case {
        return path.starts_with(&parent);
    }
    let mut inner = path.components();
    parent.components().all(|p| {
        inner.next().is_some_and(|c| {
            c.as_os_str()
                .as_encoded_bytes()
                .eq_ignore_ascii_case(p.as_os_str().as_encoded_bytes())
        })
    })
}

/// `path` with `.` dropped and each `..` removing the component before it,
/// without reading the disk. A `..` with nothing left to remove is kept
/// (it cannot climb above the root, which [`Path::components`] already
/// keeps first).
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other),
        }
    }
    out
}

/// The checks every configured path goes through before it is used: no
/// control characters ([`ensure_no_control`]), no surrounding quotes
/// ([`ensure_unquoted`]), no surrounding whitespace ([`ensure_trimmed`]),
/// no unexpanded variable ([`ensure_no_variable`]) and no Windows drive
/// path off Windows ([`ensure_native`]). `what` names the setting for the
/// error.
fn check_setting(what: &str, path: &Path) -> Result<()> {
    ensure_no_control(what, path)?;
    ensure_unquoted(what, path)?;
    ensure_trimmed(what, path)?;
    ensure_no_variable(what, path)?;
    ensure_native(what, path, cfg!(windows))
}

/// Refuses a path that names an environment variable instead of its value:
/// `$HOME/Games/title`, `${XDG_DATA_HOME}/victory` or
/// `%LOCALAPPDATA%\victory` (the form `victory.example.toml` uses for the
/// defaults, so an easy one to copy). Only a shell (or `cmd` for `%...%`)
/// expands these; TOML, an env file read by `docker run --env-file` or
/// systemd, and a launcher or IDE run configuration keep them literally.
/// The path is then relative, so it would be taken as a folder named
/// `$HOME` under the config file's folder or the working directory (often
/// the repository) and reported later only as "does not exist" or "inside
/// the repository". Real folders like `$Recycle.Bin` or `100%` are left
/// alone: a `$` part must be a whole variable name (`$HOME`, not
/// `$HOME.old`), and `%` must enclose one.
fn ensure_no_variable(what: &str, path: &Path) -> Result<()> {
    let text = path.to_string_lossy();
    let Some(var) = text.split(['/', '\\']).find_map(variable_in) else {
        return Ok(());
    };
    anyhow::bail!(
        "{what} is {}, which names the variable {var}; variables are not expanded in \
         the config file or when the value comes from an env file or a launcher rather \
         than a shell, so write the path out in full (a leading ~ is the home directory)",
        path.display()
    );
}

/// The variable reference in one path component, as written: the whole
/// component `$NAME`, or `${NAME}` or `%NAME%` anywhere in it.
fn variable_in(part: &str) -> Option<&str> {
    let is_name = |s: &str| {
        let mut chars = s.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if part.strip_prefix('$').is_some_and(is_name) {
        return Some(part);
    }
    if let Some(start) = part.find("${") {
        let rest = &part[start + 2..];
        if let Some(end) = rest.find('}')
            && is_name(&rest[..end])
        {
            return Some(&part[start..start + 2 + end + 1]);
        }
    }
    // `%ProgramFiles(x86)%` is a real Windows variable.
    let mut pieces = part.split('%');
    pieces.next();
    let mut offset = part.find('%')?;
    for piece in pieces {
        let name = piece.replace("(x86)", "");
        let closed = offset + 1 + piece.len() < part.len();
        if closed && is_name(&name) {
            return Some(&part[offset..offset + piece.len() + 2]);
        }
        offset += piece.len() + 1;
    }
    None
}

/// Refuses a path that starts or ends with whitespace (a space, or a
/// no-break space pasted from a web page). An env file or launcher keeps
/// what a shell would split off: `VICTORY_GAME_DIR= /games/title` or a value
/// with a trailing space. A leading space makes the path relative, so it
/// would be taken as a folder named ` ` under the config file's folder or
/// the working directory (often the repository); a trailing one is reported
/// later only as "does not exist", which looks right on screen. Windows
/// strips trailing spaces from folder names, and no real dump or cache
/// folder starts or ends with one; spaces inside the name are left alone.
/// The error quotes the value so the space shows.
fn ensure_trimmed(what: &str, path: &Path) -> Result<()> {
    let text = path.to_string_lossy();
    let (start, end) = match (text.chars().next(), text.chars().next_back()) {
        (Some(s), Some(e)) => (s.is_whitespace(), e.is_whitespace()),
        _ => return Ok(()),
    };
    if !start && !end {
        return Ok(());
    }
    let place = match (start, end) {
        (true, true) => "starts and ends",
        (true, false) => "starts",
        _ => "ends",
    };
    anyhow::bail!("{what} is {text:?}, which {place} with whitespace; remove it");
}

/// Refuses a path wrapped in a matching pair of quotes (`"/games/title"` or
/// `'/games/title'`, quotes included). Only a shell removes them: an env file
/// read by `docker run --env-file`, systemd's `EnvironmentFile=` (single
/// quotes on older versions), an IDE run configuration or a Windows
/// `set VICTORY_GAME_DIR="D:\games"` keeps them as part of the value. The
/// path then starts with a quote, so it is not absolute and would be taken
/// as a folder named `"` under the working directory (often the
/// repository), reported later as "does not exist" or "inside the
/// repository". No real folder name starts and ends with the same quote,
/// and `"` cannot appear in a Windows path at all; a quote elsewhere in
/// the name (`/games/Rockstar's`) is left alone.
fn ensure_unquoted(what: &str, path: &Path) -> Result<()> {
    let bytes = path.as_os_str().as_encoded_bytes();
    let quoted = match bytes {
        [first, .., last] => first == last && matches!(first, b'"' | b'\''),
        _ => false,
    };
    if !quoted {
        return Ok(());
    }
    anyhow::bail!(
        "{what} is {}, quotes included; the quotes are not removed when the value comes \
         from an env file or a launcher rather than a shell, so remove them",
        path.display()
    );
}

/// Refuses a path holding a control character (a carriage return, newline,
/// tab, ...). No real folder name has one, but a variable read from a
/// `.env` file with Windows line endings (`set -a; . ./victory.env`, an
/// env file for a container or a Proton/Wine wrapper) ends in an invisible
/// `\r`, and a value pasted from a document can carry a tab or newline.
/// Without this the path is reported later only as "does not exist", which
/// looks right on screen; the error quotes the value so the character shows.
fn ensure_no_control(what: &str, path: &Path) -> Result<()> {
    let text = path.to_string_lossy();
    let Some(c) = text.chars().find(|c| c.is_control()) else {
        return Ok(());
    };
    let hint = if c == '\r' {
        " (a Windows line ending: was it read from a file saved with CRLF line endings?)"
    } else {
        ""
    };
    anyhow::bail!(
        "{what} is {text:?}, which contains the control character {c:?}{hint}; remove it"
    );
}

/// Refuses a Windows drive path (`D:\games`, `D:/games`, `D:`) when the
/// host is not Windows (`on_windows` is `cfg!(windows)`, a parameter so the
/// check can be tested anywhere). There it is not absolute, so it would be
/// taken as a relative path: under the config file's folder, or the working
/// directory for a variable, which is often the repository. The example
/// config ships such a path, so copying it unedited on Linux is an easy way
/// to get here; the error names the setting instead of a confusing
/// "inside the repository" or "does not exist" later on.
fn ensure_native(what: &str, path: &Path, on_windows: bool) -> Result<()> {
    if on_windows || !is_drive_path(path) {
        return Ok(());
    }
    anyhow::bail!(
        "{what} is {}, a Windows path, but this host is not Windows; set it to a \
         path on this machine",
        path.display()
    );
}

/// `X:` followed by nothing, `\` or `/`.
fn is_drive_path(path: &Path) -> bool {
    match path.as_os_str().as_encoded_bytes() {
        [drive, b':'] => drive.is_ascii_alphabetic(),
        [drive, b':', sep, ..] => drive.is_ascii_alphabetic() && matches!(sep, b'\\' | b'/'),
        _ => false,
    }
}

/// `<root>/<sub>`, created. `sub` must be a plain relative path (names
/// only: no `..`, `.`, root or drive): [`Path::join`] with an absolute `sub`
/// would replace `root` outright, and a `..` would climb out of it, so a
/// title id or other computed part that came out wrong would write outside
/// the cache or data folder, possibly into the dump, without an error.
fn subdir(root: &Path, sub: &str) -> Result<PathBuf> {
    ensure_plain_subdir(sub)?;
    let dir = root.join(sub);
    ensure_outside_repo(&dir)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Refuses a `sub` for [`subdir`] that is empty or is not made only of
/// ordinary names.
fn ensure_plain_subdir(sub: &str) -> Result<()> {
    let path = Path::new(sub);
    let plain = !sub.is_empty()
        && !is_drive_path(path)
        && path.components().all(|c| matches!(c, Component::Normal(_)))
        // `Path::components` drops interior `.`; refuse it all the same.
        && !sub.split(['/', '\\']).any(|part| part == "." || part == "..");
    if !plain {
        anyhow::bail!(
            "internal error: subfolder {sub:?} is not a plain relative path; it would \
             leave the cache or data folder"
        );
    }
    Ok(())
}

/// The user's home directory: `HOME`, else `USERPROFILE` (Windows), when
/// set to an absolute path.
fn home_dir(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(|k| env(k).map(PathBuf::from))
        .find(|p| p.is_absolute())
}

/// `~` and `~/rest` (`~\rest` on Windows) under `home`. Neither TOML nor a
/// quoted shell value expands `~`, so without this `game_dir = "~/games"`
/// would name a folder called `~`. `~user` is left alone, and so is
/// everything when there is no home directory.
fn expand_home(path: PathBuf, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else { return path };
    let mut parts = path.components();
    match parts.next() {
        Some(Component::Normal(first)) if first == "~" => {
            let rest = parts.as_path();
            if rest.as_os_str().is_empty() {
                home.to_path_buf()
            } else {
                home.join(rest)
            }
        }
        _ => path,
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Cache,
    Data,
}

/// `%LOCALAPPDATA%/victory/{cache,data}`. Without `LOCALAPPDATA` (a native
/// non-Windows host), the XDG base directories: the cache under
/// `$XDG_CACHE_HOME` (`~/.cache`), the data under `$XDG_DATA_HOME`
/// (`~/.local/share`), so clearing the user's cache does not take what titles
/// must keep with it.
///
/// A variable that is set but empty or relative is skipped, as the XDG base
/// directory spec asks: otherwise `XDG_CACHE_HOME=` (or a relative value)
/// would put the directory under the working directory, which is often the
/// repository itself and so refused by the asset policy.
fn default_dir(kind: Kind, env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<PathBuf> {
    let absolute = |k: &str| env(k).map(PathBuf::from).filter(|p| p.is_absolute());
    if let Some(local) = absolute("LOCALAPPDATA") {
        let name = match kind {
            Kind::Cache => "cache",
            Kind::Data => "data",
        };
        return Ok(local.join("victory").join(name));
    }
    let (xdg, fallback) = match kind {
        Kind::Cache => ("XDG_CACHE_HOME", Path::new(".cache")),
        Kind::Data => ("XDG_DATA_HOME", Path::new(".local/share")),
    };
    let base = absolute(xdg)
        .or_else(|| absolute("HOME").map(|h| h.join(fallback)))
        .context("cannot determine a user directory; set VICTORY_CACHE_DIR and VICTORY_DATA_DIR")?;
    Ok(base.join("victory"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn environment_overrides_file() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            cache_dir: Some("cache-from-file".into()),
            data_dir: Some("data-from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| match k {
            "VICTORY_GAME_DIR" => Some(OsString::from("from-env")),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-env"));
        assert_eq!(cfg.cache_dir, PathBuf::from("cache-from-file"));
    }

    #[test]
    fn empty_env_values_are_ignored() {
        let file = FilePaths {
            game_dir: Some("from-file".into()),
            ..Default::default()
        };
        let env = |k: &str| match k {
            "VICTORY_GAME_DIR" => Some(OsString::new()),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.game_dir.unwrap(), PathBuf::from("from-file"));
    }

    fn defaults(vars: &[(&str, OsString)]) -> Config {
        let env = |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
        Config::resolve(FilePaths::default(), None, env).unwrap()
    }

    #[test]
    fn local_app_data_holds_cache_and_data() {
        let cfg = defaults(&[("LOCALAPPDATA", abs("l")), ("XDG_CACHE_HOME", abs("x"))]);
        let base = PathBuf::from(abs("l")).join("victory");
        assert_eq!(cfg.cache_dir, base.join("cache"));
        assert_eq!(cfg.data_dir, base.join("data"));
    }

    #[test]
    fn data_is_not_under_the_xdg_cache() {
        let cfg = defaults(&[("XDG_CACHE_HOME", abs("c")), ("XDG_DATA_HOME", abs("d"))]);
        assert_eq!(cfg.cache_dir, PathBuf::from(abs("c")).join("victory"));
        assert_eq!(cfg.data_dir, PathBuf::from(abs("d")).join("victory"));

        let cfg = defaults(&[("HOME", abs("h"))]);
        let home = PathBuf::from(abs("h"));
        assert_eq!(cfg.cache_dir, home.join(".cache").join("victory"));
        assert_eq!(cfg.data_dir, home.join(".local/share").join("victory"));
        assert!(!cfg.data_dir.starts_with(&cfg.cache_dir));
    }

    #[test]
    fn empty_or_relative_values_are_ignored() {
        let cfg = defaults(&[
            ("LOCALAPPDATA", OsString::new()),
            ("XDG_CACHE_HOME", OsString::new()),
            ("XDG_DATA_HOME", OsString::from("relative")),
            ("HOME", abs("h")),
        ]);
        let home = PathBuf::from(abs("h"));
        assert_eq!(cfg.cache_dir, home.join(".cache").join("victory"));
        assert_eq!(cfg.data_dir, home.join(".local/share").join("victory"));

        // A relative LOCALAPPDATA or XDG_CACHE_HOME falls through as well.
        let cfg = defaults(&[
            ("LOCALAPPDATA", OsString::from("relative/lad")),
            ("XDG_CACHE_HOME", OsString::from("relative/cache")),
            ("HOME", abs("h")),
        ]);
        assert_eq!(cfg.cache_dir, home.join(".cache").join("victory"));
    }

    #[test]
    fn default_dir_prefers_localappdata_then_xdg_then_home() {
        let all = |k: &str| match k {
            "LOCALAPPDATA" => Some(abs("lad")),
            "XDG_CACHE_HOME" => Some(abs("xdg")),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        assert_eq!(
            default_dir(Kind::Cache, all).unwrap(),
            PathBuf::from(abs("lad")).join("victory").join("cache")
        );
        let no_lad = |k: &str| if k == "LOCALAPPDATA" { None } else { all(k) };
        assert_eq!(
            default_dir(Kind::Cache, no_lad).unwrap(),
            PathBuf::from(abs("xdg")).join("victory")
        );
        let home_only = |k: &str| (k == "HOME").then(|| abs("home"));
        assert_eq!(
            default_dir(Kind::Cache, home_only).unwrap(),
            PathBuf::from(abs("home")).join(".cache").join("victory")
        );
    }

    #[test]
    fn no_user_directory_is_an_error() {
        let env = |_: &str| None;
        assert!(Config::resolve(FilePaths::default(), None, env).is_err());
        let nothing_usable = |k: &str| (k == "HOME").then(OsString::new);
        assert!(default_dir(Kind::Cache, nothing_usable).is_err());
        assert!(default_dir(Kind::Data, nothing_usable).is_err());
    }

    #[test]
    fn empty_test_titles_variable_falls_back_to_the_file() {
        let file = FilePaths {
            test_titles: vec!["from-file".into()],
            ..Default::default()
        };
        let env = |k: &str| match k {
            "VICTORY_TEST_TITLES" => Some(OsString::new()),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.test_titles, vec![PathBuf::from("from-file")]);
    }

    #[test]
    fn relative_file_paths_are_relative_to_the_file() {
        let text = format!(
            "[paths]\ngame_dir = 'game'\ncache_dir = {:?}\ntest_titles = ['t/one']\n",
            PathBuf::from(abs("elsewhere")).display().to_string()
        );
        let parsed: FileConfig = toml::from_str(&text).unwrap();
        let dir = PathBuf::from(abs("configs"));
        let paths = parsed.anchored_at(&dir, None).unwrap();
        assert_eq!(paths.game_dir.unwrap(), dir.join("game"));
        // Absolute paths are kept as written.
        assert_eq!(paths.cache_dir.unwrap(), PathBuf::from(abs("elsewhere")));
        assert_eq!(paths.data_dir, None);
        assert_eq!(paths.test_titles, vec![dir.join("t/one")]);
    }

    #[test]
    fn empty_file_values_count_as_unset() {
        let text = "[paths]\ngame_dir = ''\ncache_dir = \"\"\ndata_dir = ''\n\
                    test_titles = ['', 't/one', '']\n";
        let parsed: FileConfig = toml::from_str(text).unwrap();
        let dir = PathBuf::from(abs("configs"));
        let paths = parsed.anchored_at(&dir, None).unwrap();
        // Not the config file's folder.
        assert_eq!(paths.game_dir, None);
        assert_eq!(paths.cache_dir, None);
        assert_eq!(paths.data_dir, None);
        assert_eq!(paths.test_titles, vec![dir.join("t/one")]);
        // With `./victory.toml` they are not the empty path either, and the
        // defaults apply with nothing said to come from the file.
        let parsed: FileConfig = toml::from_str(text).unwrap();
        let paths = parsed.anchored_at(Path::new(""), None).unwrap();
        let env = |k: &str| match k {
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(paths, None, env).unwrap();
        assert_eq!(cfg.game_dir, None);
        assert!(cfg.cache_dir.is_absolute(), "{}", cfg.cache_dir.display());
        assert!(cfg.data_dir.is_absolute(), "{}", cfg.data_dir.display());
        assert_eq!(cfg.origins.game_dir, Origin::Default);
        assert_eq!(cfg.origins.cache_dir, Origin::Default);
        assert_eq!(cfg.origins.data_dir, Origin::Default);
        assert_eq!(cfg.test_titles, vec![PathBuf::from("t/one")]);
        assert_eq!(cfg.origins.test_titles, Origin::File);
    }

    #[test]
    fn the_default_config_file_keeps_paths_as_written() {
        // `./victory.toml` has an empty parent, so nothing changes.
        let parsed: FileConfig = toml::from_str("[paths]\ngame_dir = 'game'\n").unwrap();
        let paths = parsed.anchored_at(Path::new(""), None).unwrap();
        assert_eq!(paths.game_dir.unwrap(), PathBuf::from("game"));
    }

    #[test]
    fn a_leading_tilde_is_the_home_directory() {
        let home = PathBuf::from(abs("home"));
        let h = Some(home.as_path());
        assert_eq!(expand_home("~".into(), h), home);
        assert_eq!(
            expand_home("~/games/title".into(), h),
            home.join("games/title")
        );
        // Only a whole leading `~` component counts.
        assert_eq!(expand_home("~user/x".into(), h), PathBuf::from("~user/x"));
        assert_eq!(expand_home("a/~/b".into(), h), PathBuf::from("a/~/b"));
        // Without a home directory nothing changes.
        assert_eq!(expand_home("~/x".into(), None), PathBuf::from("~/x"));
    }

    #[test]
    fn tilde_in_the_file_is_home_not_next_to_the_file() {
        let parsed: FileConfig =
            toml::from_str("[paths]\ngame_dir = '~/games/title'\ntest_titles = ['~/t']\n").unwrap();
        let home = PathBuf::from(abs("home"));
        let paths = parsed
            .anchored_at(&PathBuf::from(abs("configs")), Some(&home))
            .unwrap();
        assert_eq!(paths.game_dir.unwrap(), home.join("games/title"));
        assert_eq!(paths.test_titles, vec![home.join("t")]);
    }

    #[test]
    fn tilde_in_variables_is_home() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let titles = format!("~/one{sep}~/two");
        let env = |k: &str| match k {
            "VICTORY_GAME_DIR" => Some(OsString::from("~/games/title")),
            "VICTORY_CACHE_DIR" => Some(OsString::from("~/cache")),
            "VICTORY_TEST_TITLES" => Some(OsString::from(&titles)),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(FilePaths::default(), None, env).unwrap();
        let home = PathBuf::from(abs("home"));
        assert_eq!(cfg.game_dir.unwrap(), home.join("games/title"));
        assert_eq!(cfg.cache_dir, home.join("cache"));
        assert_eq!(cfg.test_titles, vec![home.join("one"), home.join("two")]);
    }

    #[test]
    fn home_dir_falls_back_to_userprofile_and_skips_relative() {
        let env = |k: &str| match k {
            "HOME" => Some(OsString::from("relative")),
            "USERPROFILE" => Some(abs("profile")),
            _ => None,
        };
        assert_eq!(home_dir(env), Some(PathBuf::from(abs("profile"))));
        assert_eq!(home_dir(|_: &str| None), None);
    }

    #[test]
    fn victory_config_expands_tilde_and_ignores_empty() {
        let home = PathBuf::from(abs("home"));
        let with = |v: &'static str| move |k: &str| (k == "VICTORY_CONFIG").then(|| v.into());
        assert_eq!(
            explicit_config(with("~/victory.toml"), Some(&home)).unwrap(),
            Some(home.join("victory.toml"))
        );
        assert_eq!(explicit_config(with(""), Some(&home)).unwrap(), None);
        assert_eq!(explicit_config(|_: &str| None, Some(&home)).unwrap(), None);
        // Without a home directory the value is taken as written.
        assert_eq!(
            explicit_config(with("~/victory.toml"), None).unwrap(),
            Some(PathBuf::from("~/victory.toml"))
        );
    }

    #[test]
    fn control_characters_in_paths_are_refused() {
        // A CRLF `.env` file leaves a `\r` on the end of every value.
        let err = ensure_no_control("VICTORY_GAME_DIR", Path::new("/games/title\r"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("VICTORY_GAME_DIR"), "{err}");
        assert!(err.contains("\"/games/title\\r\""), "{err}");
        assert!(err.contains("CRLF"), "{err}");
        let err = ensure_no_control("[paths].data_dir", Path::new("/a\tb"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("'\\t'"), "{err}");
        assert!(!err.contains("CRLF"), "{err}");
        for ok in ["/games/title", "games with spaces", "~/games/café", ""] {
            assert!(ensure_no_control("x", Path::new(ok)).is_ok(), "{ok}");
        }

        // Every way in: each path variable, VICTORY_CONFIG and the file.
        for var in [
            "VICTORY_GAME_DIR",
            "VICTORY_CACHE_DIR",
            "VICTORY_DATA_DIR",
            "VICTORY_TEST_TITLES",
        ] {
            let env = |k: &str| match k {
                k if k == var => Some(OsString::from("/victory/x\r")),
                "HOME" => Some(abs("home")),
                _ => None,
            };
            let err = Config::resolve(FilePaths::default(), None, env)
                .unwrap_err()
                .to_string();
            assert!(err.contains(var), "{err}");
        }
        let env = |k: &str| (k == "VICTORY_CONFIG").then(|| OsString::from("victory.toml\r"));
        let err = explicit_config(env, None).unwrap_err().to_string();
        assert!(err.contains("VICTORY_CONFIG"), "{err}");
        let parsed: FileConfig = toml::from_str("[paths]\ncache_dir = \"cache\\n\"\n").unwrap();
        let err = parsed
            .anchored_at(Path::new("/configs"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[paths].cache_dir"), "{err}");
    }

    #[test]
    fn quoted_paths_are_refused() {
        // `docker run --env-file` keeps the quotes of `VICTORY_GAME_DIR="/games/title"`.
        let err = ensure_unquoted("VICTORY_GAME_DIR", Path::new("\"/games/title\""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("VICTORY_GAME_DIR"), "{err}");
        assert!(err.contains("\"/games/title\""), "{err}");
        assert!(err.contains("quotes included"), "{err}");
        assert!(ensure_unquoted("x", Path::new("'~/title'")).is_err());
        for ok in [
            "/games/title",
            "/games/Rockstar's",
            "'/games/title\"",
            "\"",
            "'",
            "",
            "games with spaces",
        ] {
            assert!(ensure_unquoted("x", Path::new(ok)).is_ok(), "{ok}");
        }

        // Every way in: each path variable, VICTORY_CONFIG and the file.
        for var in [
            "VICTORY_GAME_DIR",
            "VICTORY_CACHE_DIR",
            "VICTORY_DATA_DIR",
            "VICTORY_TEST_TITLES",
        ] {
            let env = |k: &str| match k {
                k if k == var => Some(OsString::from("\"/victory/x\"")),
                "HOME" => Some(abs("home")),
                _ => None,
            };
            let err = Config::resolve(FilePaths::default(), None, env)
                .unwrap_err()
                .to_string();
            assert!(err.contains(var), "{err}");
            assert!(err.contains("quotes included"), "{err}");
        }
        let env = |k: &str| (k == "VICTORY_CONFIG").then(|| OsString::from("'victory.toml'"));
        let err = explicit_config(env, None).unwrap_err().to_string();
        assert!(err.contains("VICTORY_CONFIG"), "{err}");
        let parsed: FileConfig =
            toml::from_str("[paths]\ngame_dir = '\"/games/title\"'\n").unwrap();
        let err = parsed
            .anchored_at(Path::new("/configs"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[paths].game_dir"), "{err}");
    }

    #[test]
    fn surrounding_whitespace_in_paths_is_refused() {
        let err = ensure_trimmed("VICTORY_GAME_DIR", Path::new(" /games/title"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("VICTORY_GAME_DIR"), "{err}");
        assert!(err.contains("\" /games/title\""), "{err}");
        assert!(err.contains("starts with whitespace"), "{err}");
        let err = ensure_trimmed("x", Path::new("/games/title "))
            .unwrap_err()
            .to_string();
        assert!(err.contains("ends with whitespace"), "{err}");
        let err = ensure_trimmed("x", Path::new(" a "))
            .unwrap_err()
            .to_string();
        assert!(err.contains("starts and ends"), "{err}");
        // A no-break space pasted from a web page.
        assert!(ensure_trimmed("x", Path::new("/games/title\u{a0}")).is_err());
        for ok in [
            "/games/title",
            "games with spaces",
            "/Program Files/x",
            "~/é",
            "",
        ] {
            assert!(ensure_trimmed("x", Path::new(ok)).is_ok(), "{ok}");
        }

        // Every way in: each path variable, VICTORY_CONFIG and the file.
        for var in [
            "VICTORY_GAME_DIR",
            "VICTORY_CACHE_DIR",
            "VICTORY_DATA_DIR",
            "VICTORY_TEST_TITLES",
        ] {
            let env = |k: &str| match k {
                k if k == var => Some(OsString::from(" /victory/x")),
                "HOME" => Some(abs("home")),
                _ => None,
            };
            let err = Config::resolve(FilePaths::default(), None, env)
                .unwrap_err()
                .to_string();
            assert!(err.contains(var), "{err}");
            assert!(err.contains("whitespace"), "{err}");
        }
        // The second of several test titles, after the separator.
        let sep = if cfg!(windows) { ";" } else { ":" };
        let titles = OsString::from(format!("/victory/a{sep} /victory/b"));
        let env = |k: &str| match k {
            "VICTORY_TEST_TITLES" => Some(titles.clone()),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let err = Config::resolve(FilePaths::default(), None, env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("\" /victory/b\""), "{err}");
        let env = |k: &str| (k == "VICTORY_CONFIG").then(|| OsString::from("victory.toml "));
        let err = explicit_config(env, None).unwrap_err().to_string();
        assert!(err.contains("VICTORY_CONFIG"), "{err}");
        let parsed: FileConfig = toml::from_str("[paths]\ndata_dir = ' data'\n").unwrap();
        let err = parsed
            .anchored_at(Path::new("/configs"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[paths].data_dir"), "{err}");
    }

    #[test]
    fn unexpanded_variables_in_paths_are_refused() {
        // An env file or the config file keeps `$HOME` literally.
        let err = ensure_no_variable("VICTORY_GAME_DIR", Path::new("$HOME/Games/title"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("VICTORY_GAME_DIR"), "{err}");
        assert!(err.contains("the variable $HOME;"), "{err}");
        assert!(err.contains("~ is the home directory"), "{err}");
        for (path, var) in [
            ("${XDG_DATA_HOME}/victory", "${XDG_DATA_HOME}"),
            ("/srv/${USER}-victory/cache", "${USER}"),
            ("/home/$USER/title", "$USER"),
            ("%LOCALAPPDATA%\\victory\\cache", "%LOCALAPPDATA%"),
            ("%LocalAppData%/victory", "%LocalAppData%"),
            ("%ProgramFiles(x86)%\\title", "%ProgramFiles(x86)%"),
            ("~/%USERNAME%-title", "%USERNAME%"),
        ] {
            let err = ensure_no_variable("x", Path::new(path))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("the variable {var};")),
                "{path}: {err}"
            );
        }
        // Real folder names with `$` or `%` in them.
        for ok in [
            "/games/title",
            "C:\\$Recycle.Bin\\x",
            "/mnt/share$/title",
            "/games/$HOME.old",
            "/games/100%",
            "/games/50% off/x",
            "/a/${}/b",
            "/a/$/b",
            "/a/%%/b",
            "~/games",
            "",
        ] {
            assert!(ensure_no_variable("x", Path::new(ok)).is_ok(), "{ok}");
        }

        // Every way in: each path variable, VICTORY_CONFIG and the file.
        for var in [
            "VICTORY_GAME_DIR",
            "VICTORY_CACHE_DIR",
            "VICTORY_DATA_DIR",
            "VICTORY_TEST_TITLES",
        ] {
            let env = |k: &str| match k {
                k if k == var => Some(OsString::from("$HOME/victory/x")),
                "HOME" => Some(abs("home")),
                _ => None,
            };
            let err = Config::resolve(FilePaths::default(), None, env)
                .unwrap_err()
                .to_string();
            assert!(err.contains(var), "{err}");
            assert!(err.contains("$HOME"), "{err}");
        }
        let env = |k: &str| {
            (k == "VICTORY_CONFIG").then(|| OsString::from("${XDG_CONFIG_HOME}/victory.toml"))
        };
        let err = explicit_config(env, None).unwrap_err().to_string();
        assert!(err.contains("VICTORY_CONFIG"), "{err}");
        let parsed: FileConfig =
            toml::from_str("[paths]\ndata_dir = '%LOCALAPPDATA%\\victory\\data'\n").unwrap();
        let err = parsed
            .anchored_at(Path::new("/configs"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[paths].data_dir"), "{err}");
        assert!(err.contains("%LOCALAPPDATA%"), "{err}");
    }

    #[test]
    fn victory_config_must_name_a_file() {
        let dir = std::env::temp_dir().join(format!("victory-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("victory.toml");
        let _ = std::fs::remove_file(&file);

        let err = config_file(Some(dir.clone())).unwrap_err().to_string();
        assert!(err.contains("is a folder"), "{err}");
        assert!(!err.contains("did you mean"), "{err}");

        std::fs::write(&file, "").unwrap();
        let err = config_file(Some(dir.clone())).unwrap_err().to_string();
        assert!(err.contains("did you mean"), "{err}");
        assert!(err.contains(&file.display().to_string()), "{err}");
        assert_eq!(config_file(Some(file.clone())).unwrap(), Some(file.clone()));

        let missing = dir.join("missing.toml");
        let err = config_file(Some(missing)).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn data_inside_the_cache_is_refused() {
        let env = |k: &str| match k {
            "VICTORY_CACHE_DIR" => Some(abs("v")),
            "VICTORY_DATA_DIR" => Some(PathBuf::from(abs("v")).join("data").into()),
            _ => None,
        };
        let err = Config::resolve(FilePaths::default(), None, env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("inside the cache folder"), "{err}");

        // The same folder for both counts as inside.
        let same = |k: &str| match k {
            "VICTORY_CACHE_DIR" | "VICTORY_DATA_DIR" => Some(abs("v")),
            _ => None,
        };
        assert!(Config::resolve(FilePaths::default(), None, same).is_err());

        // A sibling whose name merely starts with the cache's is fine, and so
        // is the cache inside the data folder.
        let sibling = |k: &str| match k {
            "VICTORY_CACHE_DIR" => Some(abs("v")),
            "VICTORY_DATA_DIR" => Some(abs("v-data")),
            _ => None,
        };
        assert!(Config::resolve(FilePaths::default(), None, sibling).is_ok());
        let cache_in_data = |k: &str| match k {
            "VICTORY_CACHE_DIR" => Some(PathBuf::from(abs("v")).join("cache").into()),
            "VICTORY_DATA_DIR" => Some(abs("v")),
            _ => None,
        };
        assert!(Config::resolve(FilePaths::default(), None, cache_in_data).is_ok());
    }

    #[test]
    fn data_inside_the_cache_ignores_dot_dot() {
        let with = |cache: PathBuf, data: PathBuf| {
            move |k: &str| match k {
                "VICTORY_CACHE_DIR" => Some(cache.clone().into_os_string()),
                "VICTORY_DATA_DIR" => Some(data.clone().into_os_string()),
                _ => None,
            }
        };
        let v = PathBuf::from(abs("v"));
        // `v/cache/../data` is `v/data`, a sibling of the cache: allowed.
        let sibling = with(v.join("cache"), v.join("cache").join("..").join("data"));
        assert!(Config::resolve(FilePaths::default(), None, sibling).is_ok());
        // A cache of `v/tmp/..` is `v`, which holds `v/./data`: refused.
        let inside = with(v.join("tmp").join(".."), v.join(".").join("data"));
        let err = Config::resolve(FilePaths::default(), None, inside)
            .unwrap_err()
            .to_string();
        assert!(err.contains("inside the cache folder"), "{err}");
    }

    #[test]
    fn game_folder_is_kept_apart_from_cache_and_data() {
        let with = |game: PathBuf, cache: PathBuf, data: PathBuf| {
            move |k: &str| match k {
                "VICTORY_GAME_DIR" => Some(game.clone().into_os_string()),
                "VICTORY_CACHE_DIR" => Some(cache.clone().into_os_string()),
                "VICTORY_DATA_DIR" => Some(data.clone().into_os_string()),
                _ => None,
            }
        };
        let v = PathBuf::from(abs("v"));
        let err = |env| {
            Config::resolve(FilePaths::default(), None, env)
                .unwrap_err()
                .to_string()
        };

        // The game inside (or equal to) the cache would go with it.
        let e = err(with(v.join("game"), v.clone(), v.join("..").join("data")));
        assert!(e.contains("game folder"), "{e}");
        assert!(e.contains("inside the cache folder"), "{e}");
        assert!(e.contains("(from VICTORY_GAME_DIR)"), "{e}");
        let e = err(with(v.clone(), v.clone(), PathBuf::from(abs("d"))));
        assert!(e.contains("inside the cache folder"), "{e}");

        // The cache or the data inside the game would write into the dump.
        let e = err(with(v.clone(), v.join("cache"), PathBuf::from(abs("d"))));
        assert!(e.contains("cache folder"), "{e}");
        assert!(e.contains("inside the game folder"), "{e}");
        let e = err(with(v.clone(), PathBuf::from(abs("c")), v.join("saves")));
        assert!(e.contains("data folder"), "{e}");
        assert!(e.contains("(from VICTORY_DATA_DIR)"), "{e}");
        // `.` and `..` are folded first: `v/../c` is outside `v`.
        let e = err(with(
            v.join("."),
            v.join("x").join("..").join("c"),
            v.join("..").join("d"),
        ));
        assert!(e.contains("inside the game folder"), "{e}");

        // Siblings are fine, including one whose name starts with the game's.
        let ok = with(
            v.clone(),
            PathBuf::from(abs("v-cache")),
            PathBuf::from(abs("v-data")),
        );
        assert!(Config::resolve(FilePaths::default(), None, ok).is_ok());
        let ok = with(
            v.join("game"),
            v.join("cache"),
            v.join("game").join("..").join("data"),
        );
        assert!(Config::resolve(FilePaths::default(), None, ok).is_ok());

        // Without a game folder there is nothing to check.
        let none = |k: &str| match k {
            "VICTORY_CACHE_DIR" => Some(abs("c")),
            "VICTORY_DATA_DIR" => Some(abs("d")),
            _ => None,
        };
        assert!(Config::resolve(FilePaths::default(), None, none).is_ok());
    }

    #[test]
    fn a_config_file_inside_the_dump_cannot_put_the_cache_there() {
        // `victory.toml` kept in the game folder with `cache_dir = "cache"`:
        // anchored at the file, the cache lands inside the dump.
        let dump = PathBuf::from(abs("title"));
        let parsed = parse_file("[paths]\ngame_dir = '.'\ncache_dir = 'cache'\n").unwrap();
        let paths = parsed.anchored_at(&dump, None).unwrap();
        let env = |k: &str| (k == "VICTORY_DATA_DIR").then(|| abs("d"));
        let err = Config::resolve(paths, None, env).unwrap_err().to_string();
        assert!(err.contains("inside the game folder"), "{err}");
        assert!(err.contains("(from config file)"), "{err}");
    }

    #[test]
    fn lexical_folds_dot_and_dot_dot() {
        let l = |p: &str| lexical(Path::new(p));
        assert_eq!(l("a/./b/../c"), PathBuf::from("a/c"));
        assert_eq!(l("../a/.."), PathBuf::from(".."));
        assert_eq!(l("a/../../b"), PathBuf::from("../b"));
        #[cfg(unix)]
        {
            assert_eq!(l("/../a"), PathBuf::from("/a"));
            assert_eq!(l("/x/y/../../z"), PathBuf::from("/z"));
        }
    }

    #[test]
    fn origins_name_the_setting_that_won() {
        let file = FilePaths {
            game_dir: Some(abs("file-game").into()),
            cache_dir: Some(abs("file-cache").into()),
            test_titles: vec![abs("file-title").into()],
            ..Default::default()
        };
        let env = |k: &str| match k {
            "VICTORY_GAME_DIR" => Some(abs("env-game")),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(
            cfg.origins,
            Origins {
                game_dir: Origin::Env("VICTORY_GAME_DIR"),
                cache_dir: Origin::File,
                data_dir: Origin::Default,
                test_titles: Origin::File,
            }
        );

        // Empty variables are unset, so they are not the origin either.
        let env = |k: &str| match k {
            "VICTORY_CACHE_DIR" | "VICTORY_TEST_TITLES" => Some(OsString::new()),
            "VICTORY_DATA_DIR" => Some(abs("env-data")),
            "HOME" => Some(abs("home")),
            _ => None,
        };
        let cfg = Config::resolve(FilePaths::default(), None, env).unwrap();
        assert_eq!(
            cfg.origins,
            Origins {
                game_dir: Origin::Default,
                cache_dir: Origin::Default,
                data_dir: Origin::Env("VICTORY_DATA_DIR"),
                test_titles: Origin::Default,
            }
        );
        assert_eq!(
            Origin::Env("VICTORY_DATA_DIR").to_string(),
            "VICTORY_DATA_DIR"
        );
        assert_eq!(Origin::File.to_string(), "config file");
        assert_eq!(Origin::Default.to_string(), "default");
    }

    #[test]
    fn data_inside_the_cache_names_both_origins() {
        // The cache from the file is the home folder, which holds the
        // default data folder: the error must say which setting is which.
        let file = FilePaths {
            cache_dir: Some(abs("home").into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "HOME").then(|| abs("home"));
        let err = Config::resolve(file, None, env).unwrap_err().to_string();
        assert!(err.contains("(from default)"), "{err}");
        assert!(err.contains("(from config file)"), "{err}");
    }

    #[test]
    fn windows_drive_paths_are_refused_off_windows() {
        for p in ["D:\\victory-data\\game", "d:/games", "D:"] {
            assert!(is_drive_path(Path::new(p)), "{p}");
            let err = ensure_native("[paths].game_dir", Path::new(p), false)
                .unwrap_err()
                .to_string();
            assert!(err.contains("[paths].game_dir"), "{err}");
            assert!(err.contains("Windows path"), "{err}");
            assert!(ensure_native("[paths].game_dir", Path::new(p), true).is_ok());
        }
        for p in [
            "/home/me/games",
            "games",
            "~/games",
            "DD:/x",
            "1:/x",
            "D:x",
            "",
        ] {
            assert!(!is_drive_path(Path::new(p)), "{p}");
            assert!(ensure_native("x", Path::new(p), false).is_ok());
        }
        if cfg!(windows) {
            return;
        }
        // Through the file and the variables, naming the setting.
        let parsed: FileConfig =
            toml::from_str("[paths]\ntest_titles = ['ok', 'E:\\\\t']\n").unwrap();
        let err = parsed
            .anchored_at(Path::new("/configs"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("[paths].test_titles"), "{err}");
        for var in [
            "VICTORY_GAME_DIR",
            "VICTORY_DATA_DIR",
            "VICTORY_TEST_TITLES",
        ] {
            let env = |k: &str| match k {
                k if k == var => Some(OsString::from("D:\\victory")),
                "HOME" => Some(abs("home")),
                _ => None,
            };
            let err = Config::resolve(FilePaths::default(), None, env)
                .unwrap_err()
                .to_string();
            assert!(err.contains(var), "{err}");
        }
    }

    #[test]
    fn empty_test_titles_env_is_ignored() {
        let file = FilePaths {
            cache_dir: Some(abs("cache").into()),
            data_dir: Some(abs("data").into()),
            test_titles: vec!["from-file".into()],
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_TEST_TITLES").then(OsString::new);
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(cfg.test_titles, vec![PathBuf::from("from-file")]);
    }

    #[test]
    fn test_titles_env_splits_paths() {
        let value = std::env::join_paths(["first", "second"]).unwrap();
        let file = FilePaths {
            cache_dir: Some(abs("cache").into()),
            data_dir: Some(abs("data").into()),
            ..Default::default()
        };
        let env = |k: &str| (k == "VICTORY_TEST_TITLES").then(|| value.clone());
        let cfg = Config::resolve(file, None, env).unwrap();
        assert_eq!(
            cfg.test_titles,
            vec![PathBuf::from("first"), PathBuf::from("second")]
        );
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../victory.example.toml");
        let parsed = parse_file(text).unwrap();
        assert!(parsed.paths.game_dir.is_some());
    }

    #[test]
    fn top_level_path_keys_say_where_they_belong() {
        let err = format!(
            "{:#}",
            parse_file("game_dir = '/games/title'\n").expect_err("refused")
        );
        assert!(
            err.contains("game_dir must be under the [paths] table"),
            "{err}"
        );

        // Above the header, two keys, and a [paths] table that is fine.
        let text = "cache_dir = 'c'\ntest_titles = []\n[paths]\ndata_dir = 'd'\n";
        let err = format!("{:#}", parse_file(text).expect_err("refused"));
        assert!(
            err.contains("cache_dir, test_titles must be under"),
            "{err}"
        );
        assert!(err.contains("above them"), "{err}");

        // Other mistakes keep the parser's own message.
        let err = format!("{:#}", parse_file("[path]\n").expect_err("refused"));
        assert!(err.contains("path"), "{err}");
        assert!(!err.contains("must be under"), "{err}");
        let err = format!(
            "{:#}",
            parse_file("[paths]\ngamedir = 'x'\n").expect_err("refused")
        );
        assert!(err.contains("gamedir"), "{err}");

        // A correct file still parses.
        let parsed = parse_file("[paths]\ngame_dir = '/games/title'\n").expect("parses");
        assert_eq!(parsed.paths.game_dir, Some(PathBuf::from("/games/title")));
    }

    #[test]
    fn subfolders_must_stay_inside_their_root() {
        for ok in ["images", "0000ABCD/storage", "a/b/c"] {
            assert!(ensure_plain_subdir(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "/etc",
            "../escape",
            "a/../../b",
            "a/./b",
            ".",
            "..",
            "C:\\x",
            "C:/x",
            "a\\..\\b",
        ] {
            let err = ensure_plain_subdir(bad).unwrap_err().to_string();
            assert!(err.contains("not a plain relative path"), "{bad}: {err}");
        }
        // Refused before anything is created: the root need not exist.
        let cfg = Config {
            cache_dir: PathBuf::from("/nonexistent-victory-test-root"),
            data_dir: PathBuf::from("/nonexistent-victory-test-data"),
            ..Default::default()
        };
        assert!(cfg.cache_subdir("/tmp").is_err());
        assert!(cfg.data_subdir("../x").is_err());
    }

    #[test]
    fn cache_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        let cfg = Config {
            cache_dir: inside,
            ..Default::default()
        };
        assert!(cfg.cache_subdir("x").is_err());
    }

    /// A path that is absolute on the host (`/x` is not absolute on Windows).
    #[test]
    fn folder_overlap_folds_case_only_when_asked() {
        let cache = Path::new("/Victory/Cache");
        assert!(is_within(Path::new("/victory/cache/data"), cache, true));
        assert!(is_within(Path::new("/VICTORY/CACHE"), cache, true));
        assert!(!is_within(Path::new("/victory/cache/data"), cache, false));
        assert!(is_within(Path::new("/Victory/Cache/data"), cache, false));
        // Still whole components, and still with `..` folded first.
        assert!(!is_within(Path::new("/victory/cachex"), cache, true));
        assert!(!is_within(Path::new("/victory/cache/../data"), cache, true));
        assert!(!is_within(Path::new("/victory"), cache, true));
    }

    #[test]
    fn shared_xdg_base_names_the_xdg_variables() {
        let base = abs("shared");
        let env = |k: &str| match k {
            "XDG_CACHE_HOME" | "XDG_DATA_HOME" => Some(base.clone()),
            _ => None,
        };
        let err = Config::resolve(FilePaths::default(), None, env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("XDG_DATA_HOME is inside"), "{err}");
        assert!(err.contains("VICTORY_DATA_DIR"), "{err}");
        // Overriding either folder is the way out.
        let data = abs("elsewhere");
        let env = |k: &str| match k {
            "XDG_CACHE_HOME" | "XDG_DATA_HOME" => Some(base.clone()),
            "VICTORY_DATA_DIR" => Some(data.clone()),
            _ => None,
        };
        let cfg = Config::resolve(FilePaths::default(), None, env).unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from(&data));
    }

    fn abs(p: &str) -> OsString {
        if cfg!(windows) {
            format!("C:\\{p}").into()
        } else {
            format!("/{p}").into()
        }
    }

    #[test]
    fn data_inside_repo_is_refused() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
        let cfg = Config {
            data_dir: inside,
            ..Default::default()
        };
        assert!(cfg.data_subdir("x").is_err());
    }
}
