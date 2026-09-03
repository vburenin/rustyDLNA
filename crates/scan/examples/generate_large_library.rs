//! Deterministic filesystem generator for `scripts/large-library-benchmark.sh`.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MEDIA_TEMPLATE: &[u8] = include_bytes!("../../../testdata/library/video/movie.mkv");

fn argument(index: usize, name: &str) -> Result<String, String> {
    env::args()
        .nth(index)
        .ok_or_else(|| format!("missing {name}"))
}

fn parse_count(index: usize, name: &str) -> Result<usize, String> {
    argument(index, name)?
        .parse::<usize>()
        .map_err(|error| format!("invalid {name}: {error}"))
}

fn physical_path(root: &Path, index: usize) -> PathBuf {
    root.join("physical")
        .join(format!("{:03}", index / 1_000))
        .join(format!("media-{index:08}.mkv"))
}

fn write_fake_container(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(MEDIA_TEMPLATE)
}

fn run() -> Result<(), String> {
    let root = PathBuf::from(argument(1, "output directory")?);
    let physical_files = parse_count(2, "physical file count")?;
    let aliases_per_kind = parse_count(3, "alias count")?;
    if physical_files == 0 {
        return Err("physical file count must be positive".into());
    }
    if aliases_per_kind > physical_files {
        return Err("alias count cannot exceed physical file count".into());
    }
    if root.exists()
        && fs::read_dir(&root)
            .map_err(|error| error.to_string())?
            .next()
            .is_some()
    {
        return Err(format!("output directory is not empty: {}", root.display()));
    }
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;

    for index in 0..physical_files {
        write_fake_container(&physical_path(&root, index))
            .map_err(|error| format!("physical file {index}: {error}"))?;
    }
    let hardlinks = root.join("aliases-hardlink");
    let symlinks = root.join("aliases-symlink");
    fs::create_dir_all(&hardlinks).map_err(|error| error.to_string())?;
    fs::create_dir_all(&symlinks).map_err(|error| error.to_string())?;
    for index in 0..aliases_per_kind {
        let source = physical_path(&root, index);
        fs::hard_link(&source, hardlinks.join(format!("hard-{index:08}.mkv")))
            .map_err(|error| format!("hard-link alias {index}: {error}"))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&source, symlinks.join(format!("sym-{index:08}.mkv")))
            .map_err(|error| format!("symlink alias {index}: {error}"))?;
        #[cfg(not(unix))]
        return Err("symlink aliases require Unix".into());
    }
    println!(
        "generated physical_files={physical_files} hardlink_aliases={aliases_per_kind} symlink_aliases={aliases_per_kind} paths={}",
        physical_files.saturating_add(aliases_per_kind.saturating_mul(2))
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("generate_large_library: {error}");
        std::process::exit(2);
    }
}
