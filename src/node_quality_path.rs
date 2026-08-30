use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub(crate) const CONFIG_MUTATION_LOCK_SUFFIX: &str = ".sing-box-tui-config-mutation.lock";
pub(crate) const QUALITY_WRITE_BLOCK_SUFFIX: &str = ".node-quality-writes-blocked";
pub(crate) const QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX: &str =
    ".node-quality-runtime-reload-required";
pub(crate) const QUALITY_USABILITY_PROBE_LOCK_SUFFIX: &str = ".node-quality-usability-probe.lock";
pub(crate) const QUALITY_RECONCILIATION_LOCK_SUFFIX: &str = ".node-quality-reconciliation.lock";

pub(crate) fn canonical_config_target(path: &Path) -> Result<PathBuf> {
    canonical_file_target(path, "config")
}

pub(crate) fn canonical_file_target(path: &Path, role: &str) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            return fs::canonicalize(path)
                .with_context(|| format!("failed to resolve {role} target {}", path.display()));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect {role} target {}", path.display()));
        }
    }

    let file_name = path
        .file_name()
        .with_context(|| format!("{role} path must name a file"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    canonical_file_target(parent, &format!("{role} parent")).map(|parent| parent.join(file_name))
}

pub(crate) fn config_mutation_lock_path(config_path: &Path) -> Result<PathBuf> {
    let config_target = canonical_config_target(config_path)?;
    let lock_target = canonical_file_target(
        &with_suffix(&config_target, CONFIG_MUTATION_LOCK_SUFFIX),
        "active config mutation lock",
    )?;
    if lock_target == config_target {
        bail!("active config mutation lock must not alias the active config");
    }
    Ok(lock_target)
}

pub(crate) fn node_quality_reserved_paths(database_path: &Path) -> Result<Vec<PathBuf>> {
    let database_path = canonical_file_target(database_path, "node-quality database")?;
    let paths = [
        database_path.clone(),
        with_suffix(&database_path, "-wal"),
        with_suffix(&database_path, "-shm"),
        with_suffix(&database_path, "-journal"),
        with_suffix(&database_path, QUALITY_WRITE_BLOCK_SUFFIX),
        with_suffix(&database_path, QUALITY_RUNTIME_RELOAD_FENCE_SUFFIX),
        with_suffix(&database_path, QUALITY_USABILITY_PROBE_LOCK_SUFFIX),
        with_suffix(&database_path, QUALITY_RECONCILIATION_LOCK_SUFFIX),
    ]
    .into_iter()
    .map(|path| canonical_file_target(&path, "node-quality reserved path"))
    .collect::<Result<Vec<_>>>()?;
    for left in 0..paths.len() {
        for right in (left + 1)..paths.len() {
            if paths[left] == paths[right] {
                bail!("node-quality database and reserved sidecars must not alias each other");
            }
        }
    }
    Ok(paths)
}

pub(crate) fn ensure_active_config_paths_are_distinct(
    config_path: &Path,
    database_path: &Path,
    auxiliary_paths: &[(&str, &Path)],
) -> Result<()> {
    let config_target = canonical_config_target(config_path)?;
    let config_lock = config_mutation_lock_path(&config_target)?;
    let quality_paths = node_quality_reserved_paths(database_path)?;
    let mut canonical = vec![
        ("active config".to_string(), config_target),
        ("active config mutation lock".to_string(), config_lock),
    ];
    for (label, path) in [
        "node-quality database",
        "node-quality WAL",
        "node-quality shared-memory file",
        "node-quality rollback journal",
        "node-quality write block",
        "node-quality runtime reload fence",
        "node-quality usability-probe lock",
        "node-quality reconciliation lock",
    ]
    .into_iter()
    .zip(quality_paths)
    {
        canonical.push((label.to_string(), path));
    }
    for (label, path) in auxiliary_paths {
        canonical.push(((*label).to_string(), canonical_file_target(path, label)?));
    }
    for left in 0..canonical.len() {
        for right in (left + 1)..canonical.len() {
            if canonical[left].1 == canonical[right].1 {
                bail!(
                    "{} must not alias {}",
                    canonical[left].0,
                    canonical[right].0
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn usability_probe_lock_path(database_path: &Path) -> Result<PathBuf> {
    let database_path = canonical_file_target(database_path, "node-quality database")?;
    canonical_file_target(
        &with_suffix(&database_path, QUALITY_USABILITY_PROBE_LOCK_SUFFIX),
        "node-quality usability-probe lock",
    )
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

pub(crate) fn default_benchmark_db_path_for_config(config_path: &Path) -> Result<PathBuf> {
    let config_target = canonical_config_target(config_path)?;
    benchmark_db_path_for_config_target(
        &config_target,
        env::var_os("SING_BOX_TUI_DB").as_deref().map(Path::new),
    )
}

fn benchmark_db_path_for_config_target(
    config_target: &Path,
    configured_database: Option<&Path>,
) -> Result<PathBuf> {
    let config_target = canonical_config_target(config_target)?;
    let parent = config_target
        .parent()
        .context("active config target must have a parent directory")?;
    if let Some(database_path) = configured_database {
        if database_path.as_os_str().is_empty() {
            bail!("SING_BOX_TUI_DB must not be empty");
        }
        if database_path == Path::new(":memory:") {
            bail!("SING_BOX_TUI_DB=:memory: is not valid for an active config");
        }
        let candidate = if database_path.is_absolute() {
            database_path.to_path_buf()
        } else {
            parent.join(database_path)
        };
        return validate_database_target(&config_target, &candidate);
    }

    let file_name = config_target
        .file_name()
        .context("active config target must name a file")?;
    if file_name == OsStr::new("config.json") {
        return validate_database_target(&config_target, &parent.join("singbox.sqlite3"));
    }
    let mut database_name = file_name.to_os_string();
    database_name.push(".sing-box-tui.sqlite3");
    validate_database_target(&config_target, &parent.join(database_name))
}

fn validate_database_target(config_target: &Path, database_path: &Path) -> Result<PathBuf> {
    let database_target = canonical_file_target(database_path, "node-quality database")?;
    let config_lock = config_mutation_lock_path(config_target)?;
    for reserved in node_quality_reserved_paths(&database_target)? {
        if reserved == config_target {
            bail!("node-quality storage must not overwrite the active config");
        }
        if reserved == config_lock {
            bail!("node-quality storage must not overwrite the active config mutation lock");
        }
    }
    Ok(database_target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir()
            .canonicalize()
            .expect("canonicalize temp directory")
            .join(format!("sing-box-tui-quality-path-{label}-{nonce}"))
    }

    #[test]
    fn config_json_keeps_the_legacy_adjacent_database_name() {
        let dir = temp_dir("legacy-name");
        fs::create_dir_all(&dir).expect("create config directory");

        assert_eq!(
            benchmark_db_path_for_config_target(&dir.join("config.json"), None)
                .expect("resolve database path"),
            dir.join("singbox.sqlite3")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn same_directory_configs_have_distinct_database_names() {
        let dir = temp_dir("different-names");
        fs::create_dir_all(&dir).expect("create config directory");

        let alpha = benchmark_db_path_for_config_target(&dir.join("alpha.json"), None)
            .expect("resolve alpha database");
        let beta = benchmark_db_path_for_config_target(&dir.join("beta.json"), None)
            .expect("resolve beta database");
        assert_eq!(alpha, dir.join("alpha.json.sing-box-tui.sqlite3"));
        assert_eq!(beta, dir.join("beta.json.sing-box-tui.sqlite3"));
        assert_ne!(alpha, beta);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn same_filename_in_different_directories_has_distinct_databases() {
        let root = temp_dir("different-directories");
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).expect("create first directory");
        fs::create_dir_all(&second).expect("create second directory");

        assert_ne!(
            benchmark_db_path_for_config_target(&first.join("config.json"), None)
                .expect("resolve first database"),
            benchmark_db_path_for_config_target(&second.join("config.json"), None)
                .expect("resolve second database")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn relative_override_is_resolved_from_the_canonical_config_parent() {
        let dir = temp_dir("relative-override");
        fs::create_dir_all(&dir).expect("create config directory");
        fs::create_dir(dir.join("quality")).expect("create relative database directory");

        assert_eq!(
            benchmark_db_path_for_config_target(
                &dir.join("config.json"),
                Some(Path::new("quality/custom.sqlite3")),
            )
            .expect("resolve relative override"),
            dir.join("quality/custom.sqlite3")
        );
        let absolute = dir.join("absolute.sqlite3");
        assert_eq!(
            benchmark_db_path_for_config_target(&dir.join("config.json"), Some(&absolute))
                .expect("retain absolute override"),
            absolute
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn future_targets_may_have_multiple_missing_parent_directories() {
        let dir = temp_dir("missing-parents");
        fs::create_dir_all(&dir).expect("create existing ancestor");
        let target = dir.join("future/nested/background/state.json");

        assert_eq!(
            canonical_file_target(&target, "future writer").expect("resolve future target"),
            target
        );
        assert!(!dir.join("future").exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn active_config_rejects_empty_and_in_memory_database_overrides() {
        let dir = temp_dir("invalid-overrides");
        fs::create_dir_all(&dir).expect("create config directory");
        let config = dir.join("config.json");

        for invalid in [Path::new(""), Path::new(":memory:")] {
            assert!(
                benchmark_db_path_for_config_target(&config, Some(invalid)).is_err(),
                "production resolver must reject {}",
                invalid.display()
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_config_and_two_cwd_style_paths_resolve_to_one_database() {
        let root = temp_dir("two-cwd");
        let shared = root.join("shared");
        let cwd_a = root.join("cwd-a");
        let cwd_b = root.join("cwd-b");
        fs::create_dir_all(&shared).expect("create shared config directory");
        fs::create_dir_all(&cwd_a).expect("create first working directory");
        fs::create_dir_all(&cwd_b).expect("create second working directory");

        let first_target = canonical_config_target(&cwd_a.join("../shared/config.json"))
            .expect("canonicalize first cwd-shaped path");
        let second_target = canonical_config_target(&cwd_b.join("../shared/config.json"))
            .expect("canonicalize second cwd-shaped path");
        let from_a = benchmark_db_path_for_config_target(&first_target, None)
            .expect("resolve from first cwd-shaped path");
        let from_b = benchmark_db_path_for_config_target(&second_target, None)
            .expect("resolve from second cwd-shaped path");
        assert_eq!(from_a, from_b);
        assert_eq!(from_a, shared.join("singbox.sqlite3"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_and_non_utf8_filename_resolve_stably() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;

        let dir = temp_dir("aliases-and-bytes");
        fs::create_dir_all(&dir).expect("create config directory");
        let target = dir.join("custom.json");
        let alias = dir.join("active.json");
        fs::write(&target, b"{}\n").expect("write config target");
        symlink(target.file_name().expect("target name"), &alias).expect("create config alias");
        let target = canonical_config_target(&target).expect("canonicalize real config");
        let alias = canonical_config_target(&alias).expect("canonicalize config alias");
        assert_eq!(
            benchmark_db_path_for_config_target(&target, None).expect("resolve real config"),
            benchmark_db_path_for_config_target(&alias, None).expect("resolve config alias")
        );

        let non_utf8 = dir.join(std::ffi::OsString::from_vec(vec![
            b'n', b'o', b'd', b'e', 0xff,
        ]));
        let non_utf8_target =
            canonical_config_target(&non_utf8).expect("canonicalize non-UTF8 config");
        let database = benchmark_db_path_for_config_target(&non_utf8_target, None)
            .expect("resolve non-UTF8 missing config");
        let mut expected_name = non_utf8
            .file_name()
            .expect("non-UTF8 filename")
            .to_os_string();
        expected_name.push(".sing-box-tui.sqlite3");
        assert_eq!(database, dir.join(expected_name));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn database_override_cannot_alias_config_or_its_lock() {
        let dir = temp_dir("database-config-alias");
        fs::create_dir_all(&dir).expect("create config directory");
        let config = dir.join("config.json");

        for database in [config.clone(), dir.join("./config.json")] {
            assert!(
                benchmark_db_path_for_config_target(&config, Some(&database)).is_err(),
                "database alias {} must be rejected",
                database.display()
            );
        }
        let config_lock = with_suffix(&config, CONFIG_MUTATION_LOCK_SUFFIX);
        assert!(benchmark_db_path_for_config_target(&config, Some(&config_lock)).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_config_lock_and_database_sidecar_aliases_are_rejected() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("reserved-symlink-alias");
        fs::create_dir_all(&dir).expect("create config directory");
        let config = dir.join("config.json");
        let database = dir.join("singbox.sqlite3");
        fs::write(&config, b"{}\n").expect("write config");
        fs::write(&database, b"").expect("write database candidate");

        let config_lock = with_suffix(&config, CONFIG_MUTATION_LOCK_SUFFIX);
        symlink(database.file_name().expect("database name"), &config_lock)
            .expect("alias config lock to database");
        assert!(benchmark_db_path_for_config_target(&config, None).is_err());
        fs::remove_file(&config_lock).expect("remove config lock alias");

        symlink(config.file_name().expect("config name"), &config_lock)
            .expect("alias config lock to config");
        assert!(config_mutation_lock_path(&config).is_err());
        fs::remove_file(&config_lock).expect("remove config self-alias");

        let quality_lock = with_suffix(&database, QUALITY_RECONCILIATION_LOCK_SUFFIX);
        symlink(database.file_name().expect("database name"), &quality_lock)
            .expect("alias quality lock to database");
        assert!(benchmark_db_path_for_config_target(&config, None).is_err());
        fs::remove_file(&quality_lock).expect("remove quality lock alias");

        let wal = with_suffix(&database, "-wal");
        symlink(database.file_name().expect("database name"), &wal)
            .expect("alias database WAL to database");
        assert!(benchmark_db_path_for_config_target(&config, None).is_err());
        fs::remove_file(&wal).expect("remove WAL database alias");

        symlink(config.file_name().expect("config name"), &wal)
            .expect("alias database WAL to config");
        assert!(benchmark_db_path_for_config_target(&config, None).is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
