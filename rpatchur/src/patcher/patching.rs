use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use anyhow::Result;
use gruf::grf::{GrfArchive, GrfArchiveBuilder};
use gruf::thor::{ThorArchive, ThorFileEntry};

/// Indicates the method that should be used when patching GRF files.
pub enum GrfPatchingMethod {
    OutOfPlace,
    InPlace,
}

/// Indicates the type of archive a "file" comes from.
enum MergeEntrySource {
    GrfArchive,
    ThorArchive,
}

/// Indicates the transformation that should be applied to the data when copied
/// from a GRF file to another.
enum DataTransformation {
    None,
    // DecompressZlib,
}

struct MergeEntry {
    pub source: MergeEntrySource,
    pub source_offset: u64,
    pub data_size: usize,
    pub transformation: DataTransformation,
}

/// Patches a GRF file with a THOR archive/patch.
pub fn apply_patch_to_grf<R: Read + Seek>(
    patching_method: GrfPatchingMethod,
    create_if_needed: bool,
    grf_file_path: impl AsRef<Path>,
    thor_archive: &mut ThorArchive<R>,
) -> Result<()> {
    if !grf_file_path.as_ref().exists() && create_if_needed {
        // Create a new GRF file if needed
        let new_grf = fs::File::create(&grf_file_path)?;
        GrfArchiveBuilder::create(new_grf, 2, 0)?;
    }
    match patching_method {
        GrfPatchingMethod::InPlace => apply_patch_to_grf_ip(grf_file_path, thor_archive),
        GrfPatchingMethod::OutOfPlace => apply_patch_to_grf_oop(grf_file_path, thor_archive),
    }
}

/// Patches a GRF in an in-place manner.
///
/// This is faster but produces output of bigger size and can corrupt file in
/// case of error.
fn apply_patch_to_grf_ip<R: Read + Seek>(
    grf_file_path: impl AsRef<Path>,
    thor_archive: &mut ThorArchive<R>,
) -> Result<()> {
    let mut builder = GrfArchiveBuilder::open(grf_file_path)?;
    let mut thor_entries: Vec<ThorFileEntry> = thor_archive
        .get_entries()
        .filter(|e| !e.is_internal())
        .cloned()
        .collect();
    thor_entries.sort_unstable_by(|a, b| a.offset.cmp(&b.offset));
    for entry in thor_entries {
        if entry.is_removed {
            let _ = builder.remove_file(&entry.relative_path);
        } else {
            builder.import_raw_entry_from_thor(thor_archive, entry.relative_path)?;
        }
    }
    Ok(())
}

/// Patches a GRF in an out-of-place manner.
///
/// This is safer and produces output of smaller size but slower.
fn apply_patch_to_grf_oop<R: Read + Seek>(
    grf_file_path: impl AsRef<Path>,
    thor_archive: &mut ThorArchive<R>,
) -> Result<()> {
    // Rename file to back it up
    let mut backup_file_path = grf_file_path.as_ref().to_path_buf();
    backup_file_path.set_extension("grf.bak");
    fs::rename(grf_file_path.as_ref(), &backup_file_path)?;

    // Prepare file entries that'll be used to make the patched GRF
    let mut merge_entries: HashMap<String, MergeEntry> = HashMap::new();
    // Add files from the original archive while discarding files remove in the patch
    let mut grf_archive = GrfArchive::open(&backup_file_path)?;
    for entry in grf_archive.get_entries() {
        if let Some(e) = thor_archive.get_file_entry(&entry.relative_path) {
            if e.is_removed {
                continue;
            }
        }
        merge_entries.insert(
            entry.relative_path.clone(),
            MergeEntry {
                source: MergeEntrySource::GrfArchive,
                source_offset: entry.offset,
                data_size: entry.size_compressed,
                transformation: DataTransformation::None,
            },
        );
    }
    // Add files from the patch
    for entry in thor_archive.get_entries() {
        if entry.is_removed || entry.is_internal() {
            continue;
        }
        merge_entries.insert(
            entry.relative_path.clone(),
            MergeEntry {
                source: MergeEntrySource::ThorArchive,
                source_offset: entry.offset,
                data_size: entry.size_compressed,
                transformation: DataTransformation::None,
            },
        );
    }

    {
        let grf_file = fs::File::create(grf_file_path)?;
        let mut builder = GrfArchiveBuilder::create(grf_file, 2, 0)?;
        for (relative_path, entry) in merge_entries {
            match entry.source {
                MergeEntrySource::GrfArchive => {
                    builder.import_raw_entry_from_grf(&mut grf_archive, relative_path)?;
                }
                MergeEntrySource::ThorArchive => {
                    builder.import_raw_entry_from_thor(thor_archive, relative_path)?;
                }
            }
        }
    }
    // Remove backup file once the patched GRF has been built
    Ok(fs::remove_file(backup_file_path)?)
}

/// Patches files located in the game client's directory with a THOR
/// archive/patch.
///
/// Self-update note: Windows holds an exclusive image lock on the running
/// launcher, so `File::create` over its own .exe fails with "os error 32".
/// `fs::rename`, however, is allowed even on a running executable (MoveFileEx
/// will happily move an open image, just not delete it). We exploit that
/// here: when the entry's resolved destination matches our `current_exe`, we
/// move the live binary aside to `<name>.old` before extracting. The fresh
/// binary lands at the original path, and a boot-time cleanup (see
/// `main::clean_stale_self_update_artifacts`) deletes the `.old` file the
/// next time the launcher starts.
pub fn apply_patch_to_disk<R: Read + Seek>(
    root_directory: impl AsRef<Path>,
    thor_archive: &mut ThorArchive<R>,
) -> Result<()> {
    // TODO(LinkZ): Save original files before updating/removing them in order
    // to be able to restore them in case of failure
    // TODO(LinkZ): Make async?
    let mut file_entries: Vec<ThorFileEntry> = thor_archive
        .get_entries()
        .filter(|e| !e.is_internal())
        .cloned()
        .collect();
    file_entries.sort_unstable_by(|a, b| a.offset.cmp(&b.offset));

    let current_exe = std::env::current_exe().ok().and_then(|p| canonicalize_or(&p));

    for entry in file_entries {
        let dest_path = join_windows_relative_path(root_directory.as_ref(), &entry.relative_path);
        if entry.is_removed {
            // Try to remove file and ignore errors (file might not exist)
            let _ignore = fs::remove_file(dest_path);
        } else {
            // Create parent directory if needed
            if let Some(parent_dir) = dest_path.parent() {
                fs::create_dir_all(parent_dir)?
            }
            // If we're about to overwrite our own running .exe, move it aside
            // first so File::create has a clear slot.
            stage_self_for_replace(&dest_path, current_exe.as_deref())?;
            // Extract file
            thor_archive.extract_file(&entry.relative_path, &dest_path)?;
        }
    }
    Ok(())
}

/// If `dest_path` resolves to the running launcher executable, move that
/// running image aside to `<dest>.old` so a fresh binary can be extracted in
/// its place. Idempotent — a leftover `.old` is removed first. Best-effort:
/// on any failure we leave the filesystem untouched so the subsequent
/// `File::create` returns the original "file in use" error.
fn stage_self_for_replace(dest_path: &Path, current_exe: Option<&Path>) -> Result<()> {
    let current_exe = match current_exe {
        Some(p) => p,
        None => return Ok(()),
    };
    let dest_canonical = match canonicalize_or(dest_path) {
        Some(c) => c,
        None => return Ok(()), // file doesn't exist yet — normal extract path
    };
    if dest_canonical != current_exe {
        return Ok(());
    }

    let mut old_path = dest_path.to_path_buf();
    let ext = match old_path.extension() {
        Some(e) => format!("{}.old", e.to_string_lossy()),
        None => "old".to_string(),
    };
    old_path.set_extension(ext);

    // Clean up any leftover from a prior aborted update before we move.
    let _ = fs::remove_file(&old_path);

    fs::rename(dest_path, &old_path)?;
    Ok(())
}

/// `std::fs::canonicalize` returns `\\?\C:\…` on Windows; that prefix doesn't
/// match `std::env::current_exe()` output. We normalize both sides through
/// the same function to make path equality reliable.
fn canonicalize_or(path: &Path) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

/// Utility function used to join path-like segments the same way it's done in
/// the GRF file format (Windows style).
fn join_windows_relative_path(path: &Path, windows_relative_path: &str) -> PathBuf {
    let mut result = PathBuf::from(path);
    for component in windows_relative_path.split('\\') {
        result.push(component);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use walkdir::WalkDir;

    #[test]
    fn test_apply_patch_to_disk() {
        let thor_dir_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/tests/thor");
        let temp_dir = tempdir().unwrap();
        {
            let count_files = |dir_path| {
                WalkDir::new(dir_path)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter_map(|entry| entry.metadata().ok())
                    .filter(|metadata| metadata.is_file())
                    .count()
            };
            let expected_file_path = temp_dir
                .path()
                .join("data/wav/se_subterranean_rustyengine.wav");
            let thor_archive_path = thor_dir_path.join("small.thor");
            let mut thor_archive = ThorArchive::open(&thor_archive_path).unwrap();
            let nb_of_added_files = thor_archive.file_count() - 1;

            // Before patching
            assert!(!expected_file_path.exists());
            assert_eq!(0, count_files(temp_dir.path()));

            apply_patch_to_disk(temp_dir.path(), &mut thor_archive).unwrap();

            // After patching
            assert!(expected_file_path.exists());
            assert_eq!(nb_of_added_files, count_files(temp_dir.path()));
            // TODO(LinkZ): Check content
        }
    }

    #[test]
    fn test_apply_patch_to_grf_ip_empty() {
        let grf_dir_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/tests/grf");
        let thor_dir_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/tests/thor");
        let temp_dir = tempdir().unwrap();
        let thor_archive_path = thor_dir_path.join("small.thor");
        let grf_archive_path = temp_dir.path().join("empty.grf");
        {
            fs::copy(grf_dir_path.join("200-empty.grf"), &grf_archive_path).unwrap();

            // Before patching
            let grf_archive = GrfArchive::open(&grf_archive_path).unwrap();
            assert_eq!(0, grf_archive.file_count());
            let grf_version_major = grf_archive.version_major();
            let grf_version_minor = grf_archive.version_minor();

            let mut thor_archive = ThorArchive::open(&thor_archive_path).unwrap();
            let nb_of_added_files = thor_archive.file_count() - 1;
            apply_patch_to_grf(
                GrfPatchingMethod::InPlace,
                false,
                &grf_archive_path,
                &mut thor_archive,
            )
            .unwrap();

            // After patching
            let grf_archive = GrfArchive::open(&grf_archive_path).unwrap();
            assert_eq!(nb_of_added_files, grf_archive.file_count());
            assert_eq!(grf_archive.version_major(), grf_version_major);
            assert_eq!(grf_archive.version_minor(), grf_version_minor);
        }
        assert!(patch_maintained_integrity(&thor_archive_path, &grf_archive_path).unwrap());
    }

    #[test]
    fn test_apply_patch_to_grf_ip_empty_create() {
        let thor_dir_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/tests/thor");
        let temp_dir = tempdir().unwrap();
        let grf_archive_path = temp_dir.path().join("empty.grf");
        let thor_archive_path = thor_dir_path.join("small.thor");
        {
            let mut thor_archive = ThorArchive::open(&thor_archive_path).unwrap();
            let nb_of_added_files = thor_archive.file_count() - 1;
            apply_patch_to_grf(
                GrfPatchingMethod::InPlace,
                true,
                &grf_archive_path,
                &mut thor_archive,
            )
            .unwrap();

            // After patching
            let grf_archive = GrfArchive::open(&grf_archive_path).unwrap();
            assert_eq!(nb_of_added_files, grf_archive.file_count());
            assert_eq!(grf_archive.version_major(), 2);
            assert_eq!(grf_archive.version_minor(), 0);
        }
        assert!(patch_maintained_integrity(&thor_archive_path, &grf_archive_path).unwrap());
    }

    #[test]
    fn test_apply_patch_to_grf_oop_empty() {
        let grf_dir_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/tests/grf");
        let thor_dir_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/tests/thor");
        let temp_dir = tempdir().unwrap();
        let thor_archive_path = thor_dir_path.join("small.thor");
        let grf_archive_path = temp_dir.path().join("empty.grf");
        {
            fs::copy(grf_dir_path.join("200-empty.grf"), &grf_archive_path).unwrap();

            // Before patching
            let grf_archive = GrfArchive::open(&grf_archive_path).unwrap();
            assert_eq!(0, grf_archive.file_count());
            let grf_version_major = grf_archive.version_major();
            let grf_version_minor = grf_archive.version_minor();

            let mut thor_archive = ThorArchive::open(&thor_archive_path).unwrap();
            let nb_of_added_files = thor_archive.file_count() - 1;
            apply_patch_to_grf(
                GrfPatchingMethod::OutOfPlace,
                false,
                &grf_archive_path,
                &mut thor_archive,
            )
            .unwrap();

            // After patching
            let grf_archive = GrfArchive::open(&grf_archive_path).unwrap();
            assert_eq!(nb_of_added_files, grf_archive.file_count());
            assert_eq!(grf_archive.version_major(), grf_version_major);
            assert_eq!(grf_archive.version_minor(), grf_version_minor);
        }
        assert!(patch_maintained_integrity(&thor_archive_path, &grf_archive_path).unwrap());
    }

    #[test]
    fn test_apply_patch_to_grf_oop_empty_create() {
        let thor_dir_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/tests/thor");
        let temp_dir = tempdir().unwrap();
        let thor_archive_path = thor_dir_path.join("small.thor");
        let grf_archive_path = temp_dir.path().join("empty.grf");
        {
            let mut thor_archive = ThorArchive::open(&thor_archive_path).unwrap();
            let nb_of_added_files = thor_archive.file_count() - 1;
            apply_patch_to_grf(
                GrfPatchingMethod::OutOfPlace,
                true,
                &grf_archive_path,
                &mut thor_archive,
            )
            .unwrap();

            // After patching
            let grf_archive = GrfArchive::open(&grf_archive_path).unwrap();
            assert_eq!(nb_of_added_files, grf_archive.file_count());
            assert_eq!(grf_archive.version_major(), 2);
            assert_eq!(grf_archive.version_minor(), 0);
        }
        assert!(patch_maintained_integrity(&thor_archive_path, &grf_archive_path).unwrap());
    }

    fn patch_maintained_integrity(
        thor_file_path: &PathBuf,
        grf_file_path: &PathBuf,
    ) -> Result<bool> {
        let mut thor_archive = ThorArchive::open(&thor_file_path)?;
        let mut grf_archive = GrfArchive::open(&grf_file_path)?;
        let thor_entries: Vec<ThorFileEntry> = thor_archive.get_entries().cloned().collect();
        for file_entry in thor_entries {
            if file_entry.is_internal() || file_entry.is_removed {
                continue;
            }
            let expected_content = thor_archive.read_file_content(&file_entry.relative_path)?;
            let file_content = grf_archive.read_file_content(&file_entry.relative_path)?;
            if expected_content != file_content {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
