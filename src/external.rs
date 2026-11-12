//! Things a build reads from outside the repo, which the mirror would
//! otherwise lose because it lives somewhere else:
//! - path dependencies like `path = "../other"`;
//! - cargo config files in directories above the repo.

use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// Names next to the repo root that path dependencies reach, e.g. `other`
/// for `path = "../other"`. The slot links `<slot>/other` to the real
/// directory, so the mirror at `<slot>/src` resolves `../other` the same way.
///
/// Paths escaping by more than one level can't be mapped and are an error.
pub fn sibling_deps(root: &Path, files: &[String]) -> Result<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for rel in files {
        let (base, paths) = if rel == "Cargo.toml" || rel.ends_with("/Cargo.toml") {
            (Path::new(rel).parent().unwrap_or(Path::new("")).to_path_buf(), manifest_paths(&root.join(rel)))
        } else if rel.ends_with(".cargo/config.toml") || rel.ends_with(".cargo/config") {
            // Config paths are relative to the directory holding `.cargo`.
            let cargo_dir = Path::new(rel).parent().unwrap_or(Path::new(""));
            (cargo_dir.parent().unwrap_or(Path::new("")).to_path_buf(), config_paths(&root.join(rel)))
        } else {
            continue;
        };
        for p in paths {
            if Path::new(&p).is_absolute() {
                continue;
            }
            match escape(&base.join(&p)) {
                Escape::Inside => {}
                Escape::Sibling(name) => {
                    names.insert(name);
                }
                Escape::Further => bail!(
                    "{rel}: path {p:?} reaches more than one level above the repo; cargo-slot can only map `../<dir>`"
                ),
            }
        }
    }
    Ok(names)
}

/// Cargo config files in directories above `root`, which cargo would read
/// for a build in the worktree but not in the slot. Skips `$CARGO_HOME`
/// (always read) and directories that are also above the slot (read anyway).
pub fn parent_configs(root: &Path, slot_src: &Path) -> Vec<PathBuf> {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")));
    let mut out = Vec::new();
    for dir in root.ancestors().skip(1) {
        if slot_src.starts_with(dir) {
            continue;
        }
        let dot_cargo = dir.join(".cargo");
        if cargo_home.as_deref() == Some(dot_cargo.as_path()) {
            continue;
        }
        for name in ["config.toml", "config"] {
            let p = dot_cargo.join(name);
            if p.is_file() {
                out.push(p);
                break;
            }
        }
    }
    // Cargo gives deeper directories priority; later `--config` args win.
    out.reverse();
    out
}

enum Escape {
    Inside,
    Sibling(String),
    Further,
}

/// Classify a repo-relative path after resolving `..` lexically.
fn escape(p: &Path) -> Escape {
    let mut depth: u32 = 0;
    let mut first_after_escape = None;
    let mut escaped = 0;
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if depth == 0 {
                    escaped += 1;
                    first_after_escape = None;
                } else {
                    depth -= 1;
                    if depth == 0 && escaped > 0 {
                        first_after_escape = None;
                    }
                }
            }
            Component::Normal(n) => {
                if escaped > 0 && depth == 0 {
                    first_after_escape = Some(n.to_string_lossy().into_owned());
                }
                depth += 1;
            }
            _ => {}
        }
    }
    match (escaped, first_after_escape) {
        (0, _) => Escape::Inside,
        (1, Some(name)) => Escape::Sibling(name),
        _ => Escape::Further,
    }
}

const DEP_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies", "dev_dependencies", "build_dependencies"];

fn read_toml(p: &Path) -> Option<toml::Table> {
    fs::read_to_string(p).ok()?.parse().ok()
}

/// `path` values of dependencies, patches and workspace members.
fn manifest_paths(p: &Path) -> Vec<String> {
    let Some(t) = read_toml(p) else { return Vec::new() };
    let mut out = Vec::new();
    let mut deps = |table: Option<&toml::Value>| {
        for dep in table.and_then(|v| v.as_table()).into_iter().flat_map(|t| t.values()) {
            if let Some(path) = dep.get("path").and_then(|v| v.as_str()) {
                out.push(path.to_string());
            }
        }
    };
    for name in DEP_TABLES {
        deps(t.get(*name));
    }
    for target in t.get("target").and_then(|v| v.as_table()).into_iter().flat_map(|t| t.values()) {
        for name in DEP_TABLES {
            deps(target.get(*name));
        }
    }
    if let Some(ws) = t.get("workspace") {
        deps(ws.get("dependencies"));
    }
    for source in t.get("patch").and_then(|v| v.as_table()).into_iter().flat_map(|t| t.values()) {
        deps(Some(source));
    }
    deps(t.get("replace"));
    if let Some(ws) = t.get("workspace") {
        for key in ["members", "default-members"] {
            for m in ws.get(key).and_then(|v| v.as_array()).into_iter().flatten() {
                if let Some(s) = m.as_str() {
                    out.push(s.to_string());
                }
            }
        }
    }
    if let Some(ws) = t.get("package").and_then(|p| p.get("workspace")).and_then(|v| v.as_str()) {
        out.push(ws.to_string());
    }
    out
}

/// `paths` overrides and patch paths in a cargo config file.
fn config_paths(p: &Path) -> Vec<String> {
    let Some(t) = read_toml(p) else { return Vec::new() };
    let mut out: Vec<String> = t
        .get("paths")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    for source in t.get("patch").and_then(|v| v.as_table()).into_iter().flat_map(|t| t.values()) {
        for dep in source.as_table().into_iter().flat_map(|t| t.values()) {
            if let Some(path) = dep.get("path").and_then(|v| v.as_str()) {
                out.push(path.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Vec<String>) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        for (rel, body) in files {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        let list = files.iter().map(|(r, _)| r.to_string()).collect();
        (tmp, root, list)
    }

    #[test]
    fn finds_sibling_path_deps() {
        let (_t, root, files) = setup(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n[workspace.dependencies]\nshared = { path = \"../shared\" }\n"),
            ("crates/a/Cargo.toml", "[dependencies]\nlocal = { path = \"../b\" }\nother = { path = \"../../../other/sub\" }\n[target.'cfg(unix)'.dev-dependencies]\nt = { path = \"../../../tools\" }\n"),
            ("crates/b/Cargo.toml", "[patch.crates-io]\nserde = { path = \"../../../serde\" }\n"),
            (".cargo/config.toml", "paths = [\"../override\"]\n"),
            ("README.md", "path = \"../../nope\""),
        ]);
        let names: Vec<String> = sibling_deps(&root, &files).unwrap().into_iter().collect();
        assert_eq!(names, ["other", "override", "serde", "shared", "tools"]);
    }

    #[test]
    fn rejects_deeper_escapes() {
        let (_t, root, files) = setup(&[("Cargo.toml", "[dependencies]\nx = { path = \"../../x\" }\n")]);
        assert!(sibling_deps(&root, &files).is_err());
    }

    #[test]
    fn escape_classification() {
        assert!(matches!(escape(Path::new("a/../b")), Escape::Inside));
        assert!(matches!(escape(Path::new("a/../../x/y")), Escape::Sibling(n) if n == "x"));
        assert!(matches!(escape(Path::new("../x/../y")), Escape::Sibling(n) if n == "y"));
        assert!(matches!(escape(Path::new("..")), Escape::Further));
        assert!(matches!(escape(Path::new("../../x")), Escape::Further));
    }

    #[test]
    fn parent_configs_skip_cargo_home_and_shared_ancestors() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let root = base.join("work/repo");
        fs::create_dir_all(&root).unwrap();
        for d in ["work/.cargo", ".cargo"] {
            fs::create_dir_all(base.join(d)).unwrap();
            fs::write(base.join(d).join("config.toml"), "").unwrap();
        }
        // `base` is above the slot too, so its config is read anyway.
        let slot_src = base.join("cache/slot-1/src");
        assert_eq!(parent_configs(&root, &slot_src), [base.join("work/.cargo/config.toml")]);
    }
}
