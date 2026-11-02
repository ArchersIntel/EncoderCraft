//! Restricts which filesystem locations a submitted job may read or write.

use std::path::{Path, PathBuf};

use encoder_core::{fsutil::{list_dir, DirEntry, DirListing}, JobSpec};

#[derive(Clone, Debug)]
pub struct MediaRoots(Vec<PathBuf>);

impl MediaRoots {
    /// Canonicalizes the roots; every root must exist.
    pub fn new(roots: &[PathBuf]) -> std::io::Result<Self> {
        let roots = roots.iter().map(|r| r.canonicalize()).collect::<Result<Vec<_>, _>>()?;
        Ok(Self(roots))
    }

    fn contains(&self, p: &Path) -> bool {
        self.0.iter().any(|root| p.starts_with(root))
    }

    /// Canonicalizes a file path and checks it is inside the roots.
    pub fn check_file(&self, path: &Path) -> Result<PathBuf, String> {
        if !path.is_absolute() {
            return Err("path must be absolute".into());
        }
        let canonical = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
        if !self.contains(&canonical) {
            return Err(format!("{} is outside the allowed media roots", canonical.display()));
        }
        Ok(canonical)
    }

    /// Lists a folder inside the roots; `None` lists the roots themselves.
    pub fn list(&self, path: Option<&Path>) -> Result<DirListing, String> {
        let Some(path) = path else {
            let entries = self
                .0
                .iter()
                .map(|r| DirEntry { name: r.to_string_lossy().into_owned(), is_dir: true, size: 0 })
                .collect();
            return Ok(DirListing { path: String::new(), entries, truncated: false });
        };
        if !path.is_absolute() {
            return Err("path must be absolute".into());
        }
        let canonical = path.canonicalize().map_err(|e| format!("{}: {e}", path.display()))?;
        if !self.contains(&canonical) {
            return Err(format!("{} is outside the allowed media roots", canonical.display()));
        }
        list_dir(&canonical).map_err(|e| format!("{}: {e}", canonical.display()))
    }

    /// Returns the spec with symlink-resolved paths, or a message for the client.
    pub fn validate(&self, spec: &JobSpec) -> Result<JobSpec, String> {
        if spec.filmcraft.is_some() {
            return Err("FilmCraft project sequences are rendered by FilmCraft on the machine that has it installed, not by a server".into());
        }
        if !spec.input.is_absolute() || !spec.output.is_absolute() {
            return Err("input and output must be absolute paths on the node".into());
        }
        let input = spec
            .input
            .canonicalize()
            .map_err(|e| format!("input {}: {e}", spec.input.display()))?;
        let out_name = spec.output.file_name().ok_or("output has no file name")?;
        let output = out_dir_with_missing(spec.output.parent().ok_or("output has no parent directory")?)?.join(out_name);

        if !self.contains(&input) {
            return Err(format!("input {} is outside the allowed media roots", input.display()));
        }
        if !self.contains(&output) {
            return Err(format!("output {} is outside the allowed media roots", output.display()));
        }
        if input == output {
            return Err("input and output are the same file".into());
        }
        Ok(JobSpec { input, output, ..spec.clone() })
    }
}

/// Canonicalizes the nearest existing ancestor and re-appends the folders that do not exist yet
/// (they are created when the job runs). `..` is refused in the missing part.
fn out_dir_with_missing(dir: &Path) -> Result<PathBuf, String> {
    let mut missing: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = dir;
    loop {
        match cur.canonicalize() {
            Ok(base) => {
                let mut full = base;
                for part in missing.iter().rev() {
                    full.push(part);
                }
                return Ok(full);
            }
            Err(_) => {
                let name = cur.file_name().ok_or_else(|| format!("output directory {} does not exist", dir.display()))?;
                if name == ".." || name == "." {
                    return Err("output directory may not contain '..'".into());
                }
                missing.push(name);
                cur = cur.parent().ok_or_else(|| format!("output directory {} does not exist", dir.display()))?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use encoder_core::job::{AudioCodec, HwAccel, VideoCodec};

    use super::*;

    fn spec(input: PathBuf, output: PathBuf) -> JobSpec {
        JobSpec {
            input,
            output,
            video: VideoCodec::H264,
            accel: HwAccel::Cpu,
            quality: 23,
            audio: AudioCodec::Copy,
            overwrite: false,
            filmcraft: None,
        }
    }

    #[test]
    fn rejects_escape_via_dotdot_and_outside_roots() {
        let base = std::env::temp_dir().join(format!("encraft-paths-{}", std::process::id()));
        let root = base.join("media");
        let other = base.join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(root.join("a.mkv"), b"x").unwrap();
        std::fs::write(other.join("secret.mkv"), b"x").unwrap();

        let roots = MediaRoots::new(std::slice::from_ref(&root)).unwrap();
        assert!(roots.validate(&spec(root.join("a.mkv"), root.join("b.mp4"))).is_ok());
        assert!(roots.validate(&spec(other.join("secret.mkv"), root.join("b.mp4"))).is_err());
        assert!(roots.validate(&spec(root.join("a.mkv"), other.join("b.mp4"))).is_err());
        assert!(roots
            .validate(&spec(root.join("..").join("other").join("secret.mkv"), root.join("b.mp4")))
            .is_err());
        assert!(roots.validate(&spec(root.join("a.mkv"), root.join("a.mkv"))).is_err());
        assert!(roots.validate(&spec("rel.mkv".into(), root.join("b.mp4"))).is_err());
        // a not-yet-existing output folder inside a root is fine, outside (or via ..) is not
        assert!(roots.validate(&spec(root.join("a.mkv"), root.join("Encoded").join("b.mp4"))).is_ok());
        assert!(roots.validate(&spec(root.join("a.mkv"), other.join("Encoded").join("b.mp4"))).is_err());
        assert!(roots.validate(&spec(root.join("a.mkv"), root.join("new").join("..").join("..").join("other").join("b.mp4"))).is_err());
        let _ = std::fs::remove_dir_all(base);
    }
}
