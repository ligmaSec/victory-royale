//! Optional per-module role metadata for the donor's renderer workarounds.
//! The registry is local to the operator: no title names, shader hashes, or
//! generated data are built in. Unclassified modules take the ordinary path.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;
use std::sync::OnceLock;

#[derive(Clone, Copy)]
pub(crate) enum Role {
    AmbientResolve,
    AmbientRadiance,
    AmbientAir,
    ProbeActivation,
    RayResolve,
    Hair,
    SkinInit,
    SceneInput,
    Bake,
    Defrag,
}

impl Role {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "ambient-resolve" => Self::AmbientResolve,
            "ambient-radiance" => Self::AmbientRadiance,
            "ambient-air" => Self::AmbientAir,
            "probe-activation" => Self::ProbeActivation,
            "ray-resolve" => Self::RayResolve,
            "hair" => Self::Hair,
            "skin-init" => Self::SkinInit,
            "scene-input" => Self::SceneInput,
            "bake" => Self::Bake,
            "defrag" => Self::Defrag,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Roles(u16);

impl Roles {
    pub(crate) fn has(self, role: Role) -> bool {
        self.0 & (1 << role as u16) != 0
    }

    pub(crate) fn insert(&mut self, role: Role) {
        self.0 |= 1 << role as u16;
    }
}

fn parse(text: &str) -> io::Result<HashMap<blake3::Hash, Roles>> {
    let mut selected = HashMap::<_, Roles>::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.split_once('#').map_or(line, |(before, _)| before);
        let mut fields = line.split_ascii_whitespace();
        let Some(hash) = fields.next() else {
            continue;
        };
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid role record on line {}", i + 1),
            )
        };
        let hash = blake3::Hash::from_hex(hash).map_err(|_| invalid())?;
        let role = fields.next().and_then(Role::parse).ok_or_else(invalid)?;
        if fields.next().is_some() {
            return Err(invalid());
        }
        selected.entry(hash).or_default().insert(role);
    }
    Ok(selected)
}

fn load(path: &Path) -> anyhow::Result<HashMap<blake3::Hash, Roles>> {
    const MAX: u64 = 256 << 10;
    victory_core::ensure_outside_repo(path)?;
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(MAX + 1)
        .read_to_string(&mut text)?;
    anyhow::ensure!(
        text.len() as u64 <= MAX,
        "shader role registry exceeds 256 KiB"
    );
    Ok(parse(&text)?)
}

pub(super) fn get(hash: &blake3::Hash) -> Roles {
    static ROLES: OnceLock<Option<HashMap<blake3::Hash, Roles>>> = OnceLock::new();
    ROLES
        .get_or_init(|| {
            let path = std::env::var_os("VICTORY_SHADER_ROLES")?;
            match load(Path::new(&path)) {
                Ok(roles) => Some(roles),
                Err(error) => {
                    tracing::warn!(%error, "shader role registry not loaded");
                    None
                }
            }
        })
        .as_ref()
        .and_then(|roles| roles.get(hash).copied())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_records_match_the_full_module_identity() {
        let hair = blake3::hash(b"synthetic hair module");
        let light = blake3::hash(b"synthetic light module");
        let text = format!(
            "{} hair\n{} skin-init # also initializes\n{} ambient-resolve\n",
            hair.to_hex(),
            hair.to_hex(),
            light.to_hex()
        );
        let roles = parse(&text).unwrap();
        assert!(roles[&hair].has(Role::Hair) && roles[&hair].has(Role::SkinInit));
        assert!(!roles[&light].has(Role::Hair));
        assert!(roles[&light].has(Role::AmbientResolve));
        assert!(parse(&format!("{} hair junk", hair.to_hex())).is_err());
        assert!(parse(&format!("{} unavailable", hair.to_hex())).is_err());
    }
}
