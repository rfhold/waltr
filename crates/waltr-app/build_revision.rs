use std::path::{Path, PathBuf};
use std::process::Command;

fn git(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(directory)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(())?;
    Some(String::from_utf8(output.stdout).ok()?.trim().to_owned())
}

pub fn sanitize_revision(value: &str) -> Option<String> {
    let value = value.trim();
    (value.len() >= 7 && value.len() <= 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

pub fn revision(directory: &Path, explicit: Option<&str>) -> String {
    explicit
        .and_then(sanitize_revision)
        .or_else(|| {
            git(directory, &["rev-parse", "--verify", "HEAD"]).and_then(|s| sanitize_revision(&s))
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

pub fn watch_paths(directory: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut git_paths = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    if let Some(reference) = git(directory, &["symbolic-ref", "-q", "HEAD"]) {
        git_paths.push(reference);
    }
    for name in git_paths {
        if let Some(value) = git(directory, &["rev-parse", "--git-path", &name]) {
            paths.push(directory.join(value));
        }
    }
    // Linked worktrees and separate git directories use a .git indirection file.
    if let Some(root) = git(directory, &["rev-parse", "--show-toplevel"]) {
        let dot_git = Path::new(&root).join(".git");
        if dot_git.is_file() {
            paths.push(dot_git);
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Repo(PathBuf);
    impl Repo {
        fn new() -> Self {
            let root = std::env::temp_dir().join("opencode").join(format!(
                "waltr-build-revision-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            let repo = Self(fs::canonicalize(root).unwrap());
            repo.run(&["init", "-b", "main"]);
            repo.run(&["config", "user.name", "Fixture"]);
            repo.run(&["config", "user.email", "fixture@example.invalid"]);
            repo.run(&["config", "core.hooksPath", "/dev/null"]);
            repo.commit();
            repo
        }
        fn run(&self, args: &[&str]) -> String {
            git(&self.0, args).unwrap_or_else(|| panic!("git {args:?} failed"))
        }
        fn commit(&self) {
            self.run(&[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ]);
        }
        fn watches(&self, expected: &Path) {
            assert!(watch_paths(&self.0).contains(&expected.to_path_buf()));
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn branch_advance_and_packed_refs() {
        let repo = Repo::new();
        repo.watches(&repo.0.join(".git/HEAD"));
        repo.watches(&repo.0.join(".git/refs/heads/main"));
        repo.watches(&repo.0.join(".git/packed-refs"));
        let first = revision(&repo.0, None);
        repo.commit();
        assert_ne!(revision(&repo.0, None), first);
        repo.run(&["pack-refs", "--all", "--prune"]);
        assert!(!repo.0.join(".git/refs/heads/main").exists());
        repo.watches(&repo.0.join(".git/packed-refs"));
        let packed = revision(&repo.0, None);
        repo.commit();
        assert_ne!(revision(&repo.0, None), packed);
        repo.watches(&repo.0.join(".git/refs/heads/main"));
    }

    #[test]
    fn tag_checkout_and_detached_head() {
        let repo = Repo::new();
        let first = revision(&repo.0, None);
        repo.run(&["-c", "tag.gpgsign=false", "tag", "fixture-tag"]);
        repo.commit();
        repo.run(&["checkout", "--detach", "fixture-tag"]);
        assert_eq!(revision(&repo.0, None), first);
        repo.watches(&repo.0.join(".git/HEAD"));
        assert_eq!(watch_paths(&repo.0).len(), 2);
        repo.commit();
        assert_ne!(revision(&repo.0, None), first);
    }

    #[test]
    fn linked_worktree_uses_private_head_and_common_refs() {
        let repo = Repo::new();
        let worktree = repo.0.join("linked");
        repo.run(&[
            "worktree",
            "add",
            "-b",
            "linked",
            worktree.to_str().unwrap(),
        ]);
        let paths = watch_paths(&worktree);
        assert!(paths.contains(&repo.0.join(".git/worktrees/linked/HEAD")));
        assert!(paths.contains(&repo.0.join(".git/refs/heads/linked")));
        assert!(paths.contains(&repo.0.join(".git/packed-refs")));
        assert!(paths.contains(&worktree.join(".git")));
        let first = revision(&worktree, None);
        git(
            &worktree,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "linked",
            ],
        )
        .unwrap();
        assert_ne!(revision(&worktree, None), first);
        assert_eq!(revision(&repo.0, None), first);
    }

    #[test]
    fn separate_git_directory_and_nested_manifest() {
        let repo = Repo::new();
        let git_directory = repo.0.join("metadata");
        repo.run(&[
            "init",
            "--separate-git-dir",
            git_directory.to_str().unwrap(),
        ]);
        assert!(repo.0.join(".git").is_file());
        let manifest = repo.0.join("crates/app");
        fs::create_dir_all(&manifest).unwrap();
        let paths = watch_paths(&manifest);
        assert!(paths.contains(&git_directory.join("HEAD")));
        assert!(paths.contains(&git_directory.join("refs/heads/main")));
        assert!(paths.contains(&git_directory.join("packed-refs")));
        assert!(paths.contains(&repo.0.join(".git")));
        assert_eq!(revision(&manifest, None), revision(&repo.0, None));
    }

    #[test]
    fn override_is_sanitized_and_invalid_override_falls_back() {
        let repo = Repo::new();
        assert_eq!(revision(&repo.0, Some(" ABCDEF123 \n")), "abcdef123");
        assert_eq!(
            revision(&repo.0, Some("not-a-revision")),
            revision(&repo.0, None)
        );
        assert_eq!(revision(&repo.0.join("missing"), None), "unknown");
        assert_eq!(
            revision(&repo.0.join("missing"), Some("abcdef1")),
            "abcdef1"
        );
        for invalid in [
            "",
            "123456",
            "123456g",
            "1234567\ncargo:bad",
            &"a".repeat(41),
        ] {
            assert!(sanitize_revision(invalid).is_none());
        }
    }
}
