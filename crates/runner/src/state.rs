//! What outlives a container: Claude's transcripts and the working directory, packed into
//! one gzip'd tar that the Durable Object stores in R2. Also the bookkeeping for output
//! files ("artifacts") the agent produces during a turn.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, BufReader, BufWriter, Write},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use agent_proto::Artifact;
use anyhow::Context;
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use tempfile::NamedTempFile;

use crate::config::Config;

const PROJECTS_PREFIX: &str = "claude-projects";
const WORK_PREFIX: &str = "work";
/// Directory levels below the output directory that are searched for artifacts.
const MAX_ARTIFACT_DEPTH: usize = 8;

fn roots(cfg: &Config) -> [(&'static str, PathBuf); 2] {
    [
        (PROJECTS_PREFIX, cfg.projects_dir()),
        (WORK_PREFIX, cfg.work_dir.clone()),
    ]
}

/// Packs the persistent directories into a temporary archive. Blocking.
pub fn export(cfg: &Config) -> anyhow::Result<NamedTempFile> {
    let file = NamedTempFile::new_in(&cfg.runtime_dir).context("creating state archive")?;
    let encoder = GzEncoder::new(BufWriter::new(file.reopen()?), Compression::fast());
    let mut archive = tar::Builder::new(encoder);
    // Store symlinks as symlinks, so a link pointing out of the working directory cannot
    // pull other files into the archive.
    archive.follow_symlinks(false);
    for (prefix, dir) in roots(cfg) {
        if dir.is_dir() {
            let left_out = pack(&mut archive, prefix, &dir)
                .with_context(|| format!("archiving {}", dir.display()))?;
            for path in left_out {
                tracing::warn!(path = %path.display(), "left out of the saved state");
            }
        }
    }
    archive.into_inner()?.finish()?.flush()?;

    let size = file.as_file().metadata()?.len();
    anyhow::ensure!(
        size <= cfg.state_max_bytes,
        "state archive is {size} bytes, over the {} byte limit",
        cfg.state_max_bytes
    );
    Ok(file)
}

/// Adds `dir` and all that is in it to `archive`, as `name`. Returns what was left out:
/// whatever is not a file, a directory or a link, and what cannot be read.
///
/// The agent runs what it likes in its working directory, and a program can leave a
/// socket there, or a file closed to its own user. An archive has no entry for the
/// first and the second cannot be opened. One such thing must not cost the conversation
/// its checkpoint, turn after turn for as long as it is there: the next container does
/// without it, as it does without the program that left it.
fn pack<W: Write>(
    archive: &mut tar::Builder<W>,
    name: &str,
    dir: &Path,
) -> io::Result<Vec<PathBuf>> {
    let mut left_out = Vec::new();
    archive.append_dir(name, dir)?;
    let mut dirs = vec![(PathBuf::from(name), dir.to_path_buf())];
    while let Some((name, dir)) = dirs.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                if !gone(&error) {
                    left_out.push(dir);
                }
                continue;
            }
        };
        for entry in entries.flatten() {
            let (name, path) = (name.join(entry.file_name()), entry.path());
            // Of the entry itself: a link is kept as the link it is.
            let Ok(kind) = entry.file_type() else {
                left_out.push(path);
                continue;
            };
            let added = if kind.is_dir() {
                archive
                    .append_dir(&name, &path)
                    .map(|()| dirs.push((name, path)))
            } else if kind.is_symlink() {
                archive.append_path_with_name(&path, &name)
            } else if kind.is_file() {
                match File::open(&path) {
                    Ok(mut file) => archive.append_file(&name, &mut file),
                    Err(error) if gone(&error) => Ok(()),
                    Err(_) => {
                        left_out.push(path);
                        Ok(())
                    }
                }
            } else {
                left_out.push(path);
                Ok(())
            };
            match added {
                Err(error) if !gone(&error) => return Err(error),
                _ => {}
            }
        }
    }
    Ok(left_out)
}

/// Whether `error` says that something is not there. What a program still running took
/// away again between being listed and being read is not there to keep.
fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

/// Unpacks an archive made by [`export`] into the persistent directories. Blocking.
pub fn import(cfg: &Config, archive: &Path) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(BufReader::new(File::open(archive)?)));
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);
    let roots = roots(cfg);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let mut components = path.components();
        let Some(Component::Normal(prefix)) = components.next() else {
            continue;
        };
        let Some((_, root)) = roots.iter().find(|(name, _)| prefix == *name) else {
            continue;
        };
        fs::create_dir_all(root)?;
        let relative = components.as_path();
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = contained_target(root, relative)
            .with_context(|| format!("refusing to unpack {}", path.display()))?;
        entry
            .unpack(&target)
            .with_context(|| format!("unpacking {}", path.display()))?;
    }
    Ok(())
}

/// Resolves `relative` under `root`, creating parent directories, and refuses any path
/// that would leave `root`: `..` components or a symlink in a parent position.
fn contained_target(root: &Path, relative: &Path) -> io::Result<PathBuf> {
    let escape = || io::Error::new(io::ErrorKind::InvalidInput, "path escapes its root");
    let mut current = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(escape());
        };
        current.push(name);
        if components.peek().is_none() {
            break;
        }
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(escape()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&current)?,
            Err(error) => return Err(error),
        }
    }
    Ok(current)
}

/// Regular files under the output directory, keyed by `/`-separated relative path.
pub type Listing = BTreeMap<String, (u64, SystemTime)>;

/// Lists the output directory. Blocking. Unreadable entries and symlinks are skipped.
pub fn scan(out_dir: &Path) -> Listing {
    fn walk(dir: &Path, prefix: &str, depth: usize, listing: &mut Listing) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let (Ok(meta), Ok(name)) = (entry.metadata(), entry.file_name().into_string()) else {
                continue;
            };
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            // `DirEntry::metadata` does not follow symlinks, so links are neither.
            if meta.is_file() {
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                listing.insert(relative, (meta.len(), modified));
            } else if meta.is_dir() && depth < MAX_ARTIFACT_DEPTH {
                walk(&entry.path(), &relative, depth + 1, listing);
            }
        }
    }
    let mut listing = Listing::new();
    walk(out_dir, "", 0, &mut listing);
    listing
}

/// Files that are new or modified in `after` compared to `before`.
pub fn changed(before: &Listing, after: &Listing) -> Vec<Artifact> {
    after
        .iter()
        .filter(|(path, stat)| before.get(*path) != Some(stat))
        .map(|(path, (size, _))| Artifact {
            path: path.clone(),
            size: *size,
        })
        .collect()
}

/// Maps an artifact path from a URL to a file, refusing anything outside `out_dir`.
pub fn resolve_artifact(out_dir: &Path, relative: &str) -> Option<PathBuf> {
    let out_dir = out_dir.canonicalize().ok()?;
    let file = out_dir.join(relative).canonicalize().ok()?;
    (file.starts_with(&out_dir) && file.is_file()).then_some(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_reports_new_and_modified_files_only() {
        let t0 = SystemTime::UNIX_EPOCH;
        let t1 = t0 + std::time::Duration::from_secs(1);
        let before: Listing = [("same".into(), (1, t0)), ("edited".into(), (1, t0))].into();
        let after: Listing = [
            ("same".into(), (1, t0)),
            ("edited".into(), (1, t1)),
            ("new/report.pdf".into(), (9, t1)),
        ]
        .into();
        let paths: Vec<_> = changed(&before, &after)
            .into_iter()
            .map(|a| a.path)
            .collect();
        assert_eq!(paths, ["edited", "new/report.pdf"]);
    }

    #[test]
    fn artifacts_cannot_escape_the_output_directory() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        fs::create_dir(&out).unwrap();
        fs::write(out.join("ok.pdf"), b"x").unwrap();
        fs::write(dir.path().join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), out.join("link")).unwrap();

        assert!(resolve_artifact(&out, "ok.pdf").is_some());
        assert!(resolve_artifact(&out, "../secret").is_none());
        assert!(resolve_artifact(&out, "link").is_none());
        assert!(resolve_artifact(&out, "missing").is_none());
    }

    #[test]
    fn packing_leaves_out_what_an_archive_cannot_hold() {
        use std::os::unix::{
            fs::{PermissionsExt, symlink},
            net::UnixListener,
        };

        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir_all(work.join("sub")).unwrap();
        fs::write(work.join("notes.md"), b"kept").unwrap();
        fs::write(work.join("sub/deep.txt"), b"kept too").unwrap();
        symlink("notes.md", work.join("link")).unwrap();
        // What a program the agent ran can leave behind: a socket it listened on, and a
        // file that not even its owner may read.
        let _socket = UnixListener::bind(work.join("sub/agent.sock")).unwrap();
        let closed = work.join("closed");
        fs::write(&closed, b"x").unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        // Nothing is closed to root, which keeps the file then.
        let readable = File::open(&closed).is_ok();

        let mut archive = tar::Builder::new(Vec::new());
        archive.follow_symlinks(false);
        let mut left_out = pack(&mut archive, "work", &work).unwrap();
        left_out.sort();
        let mut expected = vec![work.join("sub/agent.sock")];
        if !readable {
            expected.insert(0, closed);
        }
        assert_eq!(left_out, expected);

        let bytes = archive.into_inner().unwrap();
        let mut packed = tar::Archive::new(bytes.as_slice());
        let mut names: Vec<String> = packed
            .entries()
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path().unwrap().display().to_string();
                path.trim_end_matches('/').to_owned()
            })
            .filter(|name| name != "work/closed")
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "work",
                "work/link",
                "work/notes.md",
                "work/sub",
                "work/sub/deep.txt"
            ]
        );
    }

    #[test]
    fn unpacking_refuses_to_follow_symlinked_parents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        assert!(contained_target(&root, Path::new("a/b/file")).is_ok());
        assert!(root.join("a/b").is_dir());
        assert!(contained_target(&root, Path::new("link/file")).is_err());
        assert!(contained_target(&root, Path::new("../file")).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }
}
