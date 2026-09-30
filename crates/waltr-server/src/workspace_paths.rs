//! Startup checks for configured host-owned paths against the workspace root.

use std::{
    ffi::OsString,
    io,
    path::{Component, Path, PathBuf},
};

// Canonicalize the existing prefix so missing store/cache files and symlinked parents
// receive the same check as paths already on disk.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut suffix = Vec::<OsString>::new();
    loop {
        match current.canonicalize() {
            Ok(mut canonical) => {
                for part in suffix.iter().rev() {
                    for component in Path::new(part).components() {
                        match component {
                            Component::CurDir => {}
                            Component::ParentDir => {
                                canonical.pop();
                            }
                            other => canonical.push(other.as_os_str()),
                        }
                    }
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => return Err(error),
                    Err(metadata_error) if metadata_error.kind() != io::ErrorKind::NotFound => {
                        return Err(metadata_error);
                    }
                    _ => {}
                }
                let Some(part) = current.file_name() else {
                    return Err(error);
                };
                suffix.push(part.to_owned());
                if !current.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn overlaps(workspace: &Path, directory: &Path) -> io::Result<bool> {
    let workspace = resolve(workspace)?;
    let directory = resolve(directory)?;
    Ok(workspace.starts_with(&directory) || directory.starts_with(&workspace))
}

pub(crate) fn contains_file(workspace: &Path, file: &Path) -> io::Result<bool> {
    Ok(resolve(file)?.starts_with(resolve(workspace)?))
}

// A normal repository workspace may contain its editable .waltr/resources catalog.
// The configuration tree must not itself be used as the worker root.
pub(crate) fn inside_config(workspace: &Path, config_root: &Path) -> io::Result<bool> {
    Ok(resolve(workspace)?.starts_with(resolve(&config_root.join(".waltr"))?))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn unresolved_configured_symlink_fails_closed() {
        use std::os::unix::fs::symlink;

        let root = crate::test_support::temp_root();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        symlink(root.join("missing"), root.join("alias")).unwrap();
        assert!(overlaps(&workspace, &root.join("alias/sessions")).is_err());
        assert!(contains_file(&workspace, &root.join("alias/data.sqlite3")).is_err());
    }
}
