use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;
use std::time::SystemTime;

use crate::{library, paths};

// the binary cargo builds from the user's config
pub const BINARY: &str = "amane-shell";

// unpacks the library and writes the cargo project around the user's main.rs
pub fn prepare() -> PathBuf {
    let cache = paths::cache();

    let library_folder = cache.join("library");
    let project_folder = cache.join("project");

    library::unpack(&library_folder);

    fs::create_dir_all(&project_folder).expect("failed to create project folder");

    let source = paths::config().join("src");
    let main = source.join("main.rs");

    let main_path = main.display().to_string();
    let library_path = library_folder.display().to_string();

    // Keep user-declared crates from the editor-facing manifest and from
    // the previous cache manifest while refreshing Amane's machine-specific path.
    let config_manifest = fs::read_to_string(paths::config().join("Cargo.toml"))
        .unwrap_or_default();
    let project_manifest = fs::read_to_string(project_folder.join("Cargo.toml"))
        .unwrap_or_default();

    let manifest = manifest(
        &main_path,
        &library_path,
        &[&config_manifest, &project_manifest],
    );

    write_if_changed(&project_folder.join("Cargo.toml"), manifest.as_bytes());

    // rust-analyzer only finds amane through a Cargo.toml next to the user's src
    write_if_changed(&paths::config().join("Cargo.toml"), manifest.as_bytes());

    // the library's own lock file pins the versions amane was tested with
    let lock = fs::read(library_folder.join("Cargo.lock")).expect("failed to read library lock");

    clean_if_dependencies_changed(&lock, &project_folder);

    write_if_changed(&project_folder.join("Cargo.lock"), &lock);

    rebuild_if_swapped(&source, &main, &project_folder);

    project_folder
}

/*
 * cargo only rebuilds when a source is newer than the last build, so a config
 * moved or restored with its old timestamps would keep running the previous one.
 * hashing the contents catches that, and touching main.rs makes cargo notice
 */
fn rebuild_if_swapped(source: &Path, main: &Path, project: &Path) {
    let mut hasher = DefaultHasher::new();

    hash_folder(source, source, &mut hasher);

    if !update_stamp(&project.join("source-hash"), hasher.finish()) {
        return;
    }

    if let Ok(file) = fs::File::options().append(true).open(main) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/*
 * cargo names every build output after a hash of its dependencies and settings,
 * so a new lock leaves all the old outputs behind and target grows forever.
 * source edits reuse the same names, so only a new lock needs a clean.
 * cargo rewrites the project's Cargo.lock itself, so the library's lock is
 * remembered as a hash instead of compared directly
 */
fn clean_if_dependencies_changed(lock: &[u8], project: &Path) {
    let mut hasher = DefaultHasher::new();

    lock.hash(&mut hasher);

    if !update_stamp(&project.join("lock-hash"), hasher.finish()) {
        return;
    }

    // target is missing before the first build, so a failure here is fine
    let _ = fs::remove_dir_all(project.join("target"));
}

// remembers the hash in the stamp file, true when it differs from the last one
fn update_stamp(stamp: &Path, hash: u64) -> bool {
    let hash = hash.to_string();

    if fs::read_to_string(stamp).is_ok_and(|previous| previous == hash) {
        return false;
    }

    write_if_changed(stamp, hash.as_bytes());

    true
}

// sorted so the same files always give the same hash
fn hash_folder(root: &Path, folder: &Path, hasher: &mut DefaultHasher) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };

    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();

    paths.sort();

    for path in paths {
        path.strip_prefix(root).unwrap_or(&path).hash(hasher);

        if path.is_dir() {
            hash_folder(root, &path, hasher);
        } else if let Ok(contents) = fs::read(&path) {
            contents.hash(hasher);
        }
    }
}

/*
 * the dev build (the only one) optimises amane and its dependencies, drawing unoptimised
 * is too slow for smooth animation; only the user's own crate stays
 * unoptimised, so a save still rebuilds quickly after the first build.
 * dependencies carry no debug info and the user's crate only line tables,
 * which keeps the binary small and the link on every save short.
 * dependencies skip incremental, they only rebuild when amane updates.
 * the empty workspace keeps cargo from joining a workspace in a parent folder
 */
fn manifest(main: &str, library: &str, existing: &[&str]) -> String {
    let mut generated = format!(
        "[package]
name = \"{BINARY}\"
version = \"0.1.0\"
edition = \"2024\"

[[bin]]
name = \"{BINARY}\"
path = '{main}'

[dependencies]
amane = {{ path = '{library}' }}

[profile.dev]
debug = \"line-tables-only\"

[profile.dev.package.\"*\"]
opt-level = 3
debug = false
incremental = false

[workspace]
"
    )
    .parse::<DocumentMut>()
    .expect("failed to parse generated Cargo manifest");

    let dependencies = generated
        .get_mut("dependencies")
        .and_then(|item| item.as_table_mut())
        .expect("generated manifest has no dependencies table");

    // Config is authoritative; the cache is a migration fallback for crates
    // declared there by older Amane versions or before the config manifest existed.
    for manifest in existing {
        let Ok(parsed) = manifest.parse::<DocumentMut>() else {
            continue;
        };

        let Some(additional) = parsed.get("dependencies").and_then(|item| item.as_table()) else {
            continue;
        };

        for (name, dependency) in additional.iter() {
            if name != "amane" && !dependencies.contains_key(name) {
                dependencies.insert(name, dependency.clone());
            }
        }
    }

    generated.to_string()
}

fn write_if_changed(path: &Path, contents: &[u8]) {
    if fs::read(path).is_ok_and(|current| current == contents) {
        return;
    }

    fs::write(path, contents).expect("failed to write project file");
}
