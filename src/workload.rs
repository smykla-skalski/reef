use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const MAX_GROUPS: usize = 256;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Workload {
    pub ecosystem: String,
    pub repository_id: Option<String>,
    pub workspace_id: Option<String>,
    pub command_fingerprint: Option<String>,
}

impl Default for Workload {
    fn default() -> Self {
        Self {
            ecosystem: "unknown".to_owned(),
            repository_id: None,
            workspace_id: None,
            command_fingerprint: None,
        }
    }
}

impl Workload {
    pub(crate) fn short(value: Option<&str>) -> &str {
        value.and_then(|s| s.get(..12)).unwrap_or("unknown")
    }

    pub(crate) fn bounded(&self, seen: &mut std::collections::BTreeSet<Self>) -> Self {
        let safe = self.sanitized();
        if seen.contains(&safe) || seen.len() < MAX_GROUPS {
            seen.insert(safe.clone());
            safe
        } else {
            Self {
                ecosystem: safe.ecosystem,
                ..Self::default()
            }
        }
    }

    pub(crate) fn sanitized(&self) -> Self {
        let opaque = |value: &Option<String>| {
            value
                .as_ref()
                .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .cloned()
        };
        Self {
            ecosystem: match self.ecosystem.as_str() {
                "Rust" => "Rust",
                "Go" => "Go",
                "Node" => "Node",
                "mixed" => "mixed",
                _ => "unknown",
            }
            .to_owned(),
            repository_id: opaque(&self.repository_id),
            workspace_id: opaque(&self.workspace_id),
            command_fingerprint: opaque(&self.command_fingerprint),
        }
    }
}

pub(crate) fn ecosystem(tool: &str) -> &'static str {
    match tool {
        "cargo" | "rustc" | "clippy-driver" => "Rust",
        "go" | "compile" | "link" | "asm" | "cgo" | "golangci-lint" | "staticcheck" => "Go",
        "node" | "npm" | "pnpm" | "yarn" | "bun" | "jest" | "vitest" | "eslint" => "Node",
        "mise" | "make" | "cc" | "gcc" | "g++" | "clang" | "clang++" => "mixed",
        _ => "unknown",
    }
}

pub(crate) fn installation_key(dir: &Path) -> io::Result<[u8; 32]> {
    let path = dir.join("workload-secret");
    let mut key = [0_u8; 32];
    match crate::observe::private_file(&path, true, false) {
        Ok(mut file) => {
            getrandom::fill(&mut key).map_err(|error| io::Error::other(error.to_string()))?;
            file.write_all(&key)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut file = crate::observe::private_file(&path, false, false)?;
            if file.metadata()?.len() != 32 {
                return Err(io::Error::other("invalid workload secret"));
            }
            file.read_exact(&mut key)?;
        }
        Err(error) => return Err(error),
    }
    Ok(key)
}

fn digest(key: &[u8; 32], domain: &[u8], parts: &[&[u8]]) -> String {
    let mut inner_pad = [0x36; 64];
    let mut outer_pad = [0x5c; 64];
    for (index, byte) in key.iter().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(domain);
    for part in parts {
        inner.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_le_bytes());
        inner.update(part);
    }
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner.finalize());
    let mut hex = String::with_capacity(64);
    for byte in outer.finalize() {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn metadata_path(base: &Path, value: &str) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() || value.len() > 4096 {
        return None;
    }
    let path = Path::new(value);
    fs::canonicalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
    .ok()
}

fn workspace(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let cwd = fs::canonicalize(cwd).ok()?;
    for root in cwd.ancestors() {
        let marker = root.join(".git");
        let git = if marker.is_dir() {
            fs::canonicalize(marker).ok()?
        } else if marker.is_file() {
            let text = read_metadata(&marker)?;
            metadata_path(root, text.strip_prefix("gitdir:")?)?
        } else {
            continue;
        };
        let common = git.join("commondir");
        let repository = if common.exists() {
            metadata_path(&git, &read_metadata(&common)?)?
        } else {
            git
        };
        return Some((root.to_path_buf(), repository));
    }
    None
}

fn read_metadata(path: &Path) -> Option<String> {
    if fs::metadata(path).ok()?.len() > 8192 {
        return None;
    }
    fs::read_to_string(path).ok()
}

pub(crate) fn identify(
    key: Option<&[u8; 32]>,
    cwd: Option<&Path>,
    tool: &str,
    args: &[OsString],
) -> Workload {
    let mut result = Workload {
        ecosystem: ecosystem(tool).to_owned(),
        ..Workload::default()
    };
    let Some((key, cwd)) = key.zip(cwd) else {
        return result;
    };
    let Some((root, repository)) = workspace(cwd) else {
        return result;
    };
    let alias_root = cwd
        .ancestors()
        .find(|path| fs::canonicalize(path).is_ok_and(|canonical| canonical == root))
        .unwrap_or(&root);
    let alias_bytes = alias_root.as_os_str().as_encoded_bytes();
    let Ok(cwd) = fs::canonicalize(cwd) else {
        return result;
    };
    result.repository_id = Some(digest(
        key,
        b"reef-repository-v1",
        &[repository.as_os_str().as_encoded_bytes()],
    ));
    result.workspace_id = Some(digest(
        key,
        b"reef-workspace-v1",
        &[root.as_os_str().as_encoded_bytes()],
    ));
    // Retain only bounded, framed keyed digests; relative cwd permits cross-worktree comparison.
    if tool == "other"
        || args.len() > 256
        || args
            .iter()
            .map(|a| a.as_encoded_bytes().len())
            .sum::<usize>()
            > 32768
    {
        return result;
    }
    let mut normalized = vec![
        tool.as_bytes().to_vec(),
        cwd.strip_prefix(&root)
            .unwrap_or(&cwd)
            .as_os_str()
            .as_encoded_bytes()
            .to_vec(),
    ];
    for arg in args {
        let bytes = arg.as_encoded_bytes();
        let root_bytes = root.as_os_str().as_encoded_bytes();
        let value = bytes
            .strip_prefix(root_bytes)
            .or_else(|| bytes.strip_prefix(alias_bytes))
            .filter(|tail| tail.first().is_some_and(|b| matches!(b, b'/' | b'\\')));
        normalized.push(value.map_or_else(
            || {
                if bytes == root_bytes || bytes == alias_bytes {
                    b"$WORKSPACE".to_vec()
                } else {
                    bytes.to_vec()
                }
            },
            |tail| [b"$WORKSPACE".as_slice(), tail].concat(),
        ));
    }
    result.command_fingerprint = Some(digest(
        key,
        b"reef-command-v1",
        &normalized.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    ));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_digest_matches_hmac_sha256_known_answer() {
        let mut key = [0; 32];
        key[..20].fill(0x0b);
        assert_eq!(
            digest(&key, b"Hi There", &[]),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    fn fixture() -> PathBuf {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let root =
            std::env::temp_dir().join(format!("reef-workload-{:x}", u64::from_ne_bytes(random)));
        fs::create_dir_all(root.join("checkout/.git/worktrees/linked")).unwrap();
        fs::create_dir_all(root.join("checkout/src")).unwrap();
        fs::create_dir_all(root.join("linked/src")).unwrap();
        fs::write(
            root.join("linked/.git"),
            "gitdir: ../checkout/.git/worktrees/linked\n",
        )
        .unwrap();
        fs::write(
            root.join("checkout/.git/worktrees/linked/commondir"),
            "../..\n",
        )
        .unwrap();
        root
    }

    #[test]
    fn linked_worktrees_share_repository_and_equivalent_command_only() {
        let root = fixture();
        let args = vec![OsString::from("test"), OsString::from("./private-package")];
        let first = identify(
            Some(&[7; 32]),
            Some(&root.join("checkout/src")),
            "go",
            &args,
        );
        let linked = identify(Some(&[7; 32]), Some(&root.join("linked/src")), "go", &args);
        assert!(first.repository_id.is_some());
        assert_eq!(first.repository_id, linked.repository_id);
        assert_ne!(first.workspace_id, linked.workspace_id);
        assert_eq!(first.command_fingerprint, linked.command_fingerprint);
        let different = identify(Some(&[7; 32]), Some(&root.join("linked")), "go", &args);
        assert_ne!(first.command_fingerprint, different.command_fingerprint);
        let other_installation = identify(
            Some(&[8; 32]),
            Some(&root.join("checkout/src")),
            "go",
            &args,
        );
        assert_ne!(first.repository_id, other_installation.repository_id);
        assert_ne!(
            first.command_fingerprint,
            other_installation.command_fingerprint
        );
        let json = serde_json::to_string(&first).unwrap();
        assert!(!json.contains("checkout"));
        assert!(!json.contains("private-package"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn normalized_absolute_workspace_paths_match_but_argv_boundaries_differ() {
        let root = fixture();
        let first = identify(
            Some(&[7; 32]),
            Some(&root.join("checkout")),
            "cargo",
            &[root.join("checkout/src").into_os_string()],
        );
        let linked = identify(
            Some(&[7; 32]),
            Some(&root.join("linked")),
            "cargo",
            &[root.join("linked/src").into_os_string()],
        );
        assert_eq!(first.command_fingerprint, linked.command_fingerprint);
        let one = digest(&[7; 32], b"test", &[b"a", b"bc"]);
        let two = digest(&[7; 32], b"test", &[b"ab", b"c"]);
        assert_ne!(one, two);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_and_unreadable_workspaces_fail_closed() {
        assert_eq!(identify(None, None, "rustc", &[]).ecosystem, "Rust");
        let root = fixture();
        fs::write(root.join("linked/.git"), "unreadable metadata").unwrap();
        assert!(
            identify(Some(&[7; 32]), Some(&root.join("linked")), "go", &[])
                .repository_id
                .is_none()
        );
        assert!(
            identify(Some(&[7; 32]), Some(&root.join("checkout")), "other", &[])
                .command_fingerprint
                .is_none()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn secret_survives_restart_and_is_installation_local() {
        let root = fixture();
        let key = installation_key(&root).unwrap();
        assert_eq!(key, installation_key(&root).unwrap());
        assert_ne!(key, installation_key(&root.join("linked")).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.join("workload-secret"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_fields_and_overflow_remain_explicitly_unknown() {
        let legacy: Workload = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy, Workload::default());
        let mut seen = std::collections::BTreeSet::new();
        for index in 0..MAX_GROUPS {
            let workload = Workload {
                command_fingerprint: Some(format!("{index:064x}")),
                ..Workload::default()
            };
            assert!(workload.bounded(&mut seen).command_fingerprint.is_some());
        }
        let overflow = Workload {
            command_fingerprint: Some(format!("{MAX_GROUPS:064x}")),
            ..Workload::default()
        };
        assert!(overflow.bounded(&mut seen).command_fingerprint.is_none());
        assert_eq!(seen.len(), MAX_GROUPS);
    }
}
