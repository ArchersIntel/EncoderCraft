//! FilmCraft `.fcproj` reader.
//!
//! Layout (from FilmCraft's `docs/project-files.md` and `crates/project`):
//! `{ "format": "filmcraft.project", "schema_version": N, "project": { "name", "items": { "<id>": {
//! "name", "kind": { "Sequence": {..} | "Media": {..} | .. } } } } }`. Time is `i64` ticks at
//! [`TICKS_PER_SECOND`]. Only the fields needed to list sequences are read; everything else is
//! ignored, so newer schema versions keep working unless these fields move.

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TICKS_PER_SECOND: i64 = 254_016_000_000;
/// Highest schema this reader was written against.
pub const KNOWN_SCHEMA: u32 = 17;
const FORMAT_ID: &str = "filmcraft.project";
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("cannot read {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{} is larger than 512 MiB; refusing to load it", .0.display())]
    TooLarge(PathBuf),
    #[error("not a valid JSON project file: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not a FilmCraft project (format is '{0}')")]
    WrongFormat(String),
    #[error("project has no 'project.items' section")]
    NoItems,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct MediaCounts {
    /// Distinct media files the sequence uses, including through nested sequences.
    pub total: usize,
    /// Of those, files that are marked offline or not found on disk.
    pub missing: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SequenceInfo {
    pub id: u64,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub fps_num: i64,
    pub fps_den: i64,
    pub duration_secs: f64,
    pub video_tracks: usize,
    pub audio_tracks: usize,
    pub clips: usize,
    /// Used as a nested sequence inside another one.
    pub nested_in_other: bool,
    /// A multi-camera source (an internal helper sequence, rarely exported on its own).
    pub multicam_source: bool,
    pub media: MediaCounts,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub path: PathBuf,
    pub name: String,
    pub generator: Option<String>,
    pub schema_version: u32,
    pub sequences: Vec<SequenceInfo>,
    pub warnings: Vec<String>,
}

// ---- tolerant raw shapes -------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct RawProject {
    #[serde(default)]
    name: String,
    items: Option<BTreeMap<String, RawItem>>,
}

#[derive(Deserialize)]
struct RawItem {
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: Value,
}

#[derive(Deserialize, Default)]
struct RawSequence {
    #[serde(default)]
    settings: RawSettings,
    #[serde(default)]
    video_tracks: Vec<RawTrack>,
    #[serde(default)]
    audio_tracks: Vec<RawTrack>,
    #[serde(default)]
    multicam: Option<Value>,
}

#[derive(Deserialize, Default)]
struct RawSettings {
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    frame_rate: RawRate,
}

#[derive(Deserialize)]
struct RawRate {
    #[serde(default)]
    num: i64,
    #[serde(default = "one")]
    den: i64,
}
impl Default for RawRate {
    fn default() -> Self {
        Self { num: 0, den: 1 }
    }
}
fn one() -> i64 {
    1
}

#[derive(Deserialize, Default)]
struct RawTrack {
    #[serde(default)]
    items: Vec<RawClip>,
}

#[derive(Deserialize)]
struct RawClip {
    #[serde(default)]
    item: u64,
    #[serde(default)]
    start: i64,
    #[serde(default)]
    duration: i64,
}

#[derive(Deserialize, Default)]
struct RawMedia {
    #[serde(default)]
    media: Value,
    #[serde(default)]
    offline: bool,
}

/// `{"Variant": content}` -> (`"Variant"`, content).
fn variant(v: &Value) -> Option<(&str, &Value)> {
    let obj = v.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    obj.iter().next().map(|(k, v)| (k.as_str(), v))
}

// ---- reader --------------------------------------------------------------------------------

pub fn read_project(path: &Path) -> Result<ProjectSummary, ImportError> {
    let io = |source| ImportError::Io { path: path.to_path_buf(), source };
    if std::fs::metadata(path).map_err(io)?.len() > MAX_FILE_BYTES {
        return Err(ImportError::TooLarge(path.to_path_buf()));
    }
    let bytes = std::fs::read(path).map_err(io)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    parse_project(&bytes, path, dir, &|p: &Path| p.exists())
}

/// `exists` is injectable so tests (and remote projects) don't depend on the local disk.
pub fn parse_project(
    bytes: &[u8],
    path: &Path,
    project_dir: &Path,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<ProjectSummary, ImportError> {
    let root: Value = serde_json::from_slice(bytes)?;
    let mut warnings = Vec::new();

    let (schema_version, generator, project_value) = match root.get("format").and_then(Value::as_str) {
        Some(f) if f == FORMAT_ID => {
            let v = root.get("schema_version").and_then(Value::as_u64).unwrap_or(0) as u32;
            let g = root.get("generator").and_then(Value::as_str).map(str::to_owned);
            (v, g, root.get("project").cloned().ok_or(ImportError::NoItems)?)
        }
        Some(other) => return Err(ImportError::WrongFormat(other.to_owned())),
        // Schema v1 was a bare Project object.
        None if root.get("items").is_some() => {
            warnings.push("Old (schema 1) project file; sequence details may be incomplete.".into());
            (1, None, root.clone())
        }
        None => return Err(ImportError::WrongFormat("missing".into())),
    };
    if schema_version > KNOWN_SCHEMA {
        warnings.push(format!(
            "Project uses schema {schema_version}, newer than the {KNOWN_SCHEMA} this importer was written for; reading it best-effort."
        ));
    }

    let project: RawProject = serde_json::from_value(project_value)?;
    let items = project.items.ok_or(ImportError::NoItems)?;

    // First pass: classify items.
    let mut sequences: BTreeMap<u64, (&str, RawSequence)> = BTreeMap::new();
    let mut media: BTreeMap<u64, (PathBuf, bool)> = BTreeMap::new();
    for (key, item) in &items {
        let Ok(id) = key.parse::<u64>() else { continue };
        match variant(&item.kind) {
            Some(("Sequence", v)) => match RawSequence::deserialize(v) {
                Ok(s) => {
                    sequences.insert(id, (item.name.as_str(), s));
                }
                Err(e) => warnings.push(format!("Sequence '{}' could not be read: {e}", item.name)),
            },
            Some(("Media", v)) => {
                if let Ok(m) = RawMedia::deserialize(v) {
                    if let Some(("File", f)) = variant(&m.media) {
                        if let Some(p) = f.get("path").and_then(Value::as_str) {
                            media.insert(id, (resolve_media(p, project_dir, exists), m.offline));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Which sequences are used inside other sequences?
    let mut nested: HashSet<u64> = HashSet::new();
    for (own_id, (_, seq)) in &sequences {
        for clip in seq.video_tracks.iter().chain(&seq.audio_tracks).flat_map(|t| &t.items) {
            if clip.item != *own_id && sequences.contains_key(&clip.item) {
                nested.insert(clip.item);
            }
        }
    }

    let mut out = Vec::with_capacity(sequences.len());
    for (&id, (name, seq)) in &sequences {
        let mut files = HashSet::new();
        collect_media(id, &sequences, &media, &mut files, &mut HashSet::new());
        let missing = files
            .iter()
            .filter(|m| media.get(m).is_some_and(|(p, offline)| *offline || !exists(p)))
            .count();

        let clips = seq.video_tracks.iter().chain(&seq.audio_tracks).map(|t| t.items.len()).sum();
        let end_ticks = seq
            .video_tracks
            .iter()
            .chain(&seq.audio_tracks)
            .flat_map(|t| &t.items)
            .map(|c| c.start.saturating_add(c.duration))
            .max()
            .unwrap_or(0)
            .max(0);
        let rate = &seq.settings.frame_rate;
        out.push(SequenceInfo {
            id,
            name: if name.is_empty() { format!("Sequence {id}") } else { (*name).to_owned() },
            width: seq.settings.width,
            height: seq.settings.height,
            fps: if rate.den > 0 { rate.num as f64 / rate.den as f64 } else { 0.0 },
            fps_num: rate.num,
            fps_den: rate.den,
            duration_secs: end_ticks as f64 / TICKS_PER_SECOND as f64,
            video_tracks: seq.video_tracks.len(),
            audio_tracks: seq.audio_tracks.len(),
            clips,
            nested_in_other: nested.contains(&id),
            multicam_source: seq.multicam.as_ref().is_some_and(|v| !v.is_null()),
            media: MediaCounts { total: files.len(), missing },
        });
    }
    // Top-level sequences first (the ones people export), then by name.
    out.sort_by(|a, b| {
        (a.multicam_source, a.nested_in_other, a.name.to_lowercase())
            .cmp(&(b.multicam_source, b.nested_in_other, b.name.to_lowercase()))
    });

    Ok(ProjectSummary {
        path: path.to_path_buf(),
        name: project.name,
        generator,
        schema_version,
        sequences: out,
        warnings,
    })
}

/// Resolves a stored media path. Relative paths are taken from the project's folder. Absolute
/// paths that no longer exist (the project was moved, or the drive letter changed) are relinked
/// by trying ever shorter tails of the stored path under the project's folder.
fn resolve_media(stored: &str, project_dir: &Path, exists: &dyn Fn(&Path) -> bool) -> PathBuf {
    let p = Path::new(stored);
    if !p.is_absolute() {
        return project_dir.join(p);
    }
    if exists(p) {
        return p.to_path_buf();
    }
    // Split on both separators: the file may have been written on another OS.
    let parts: Vec<&str> = stored
        .split(['/', '\\'])
        .filter(|c| !c.is_empty() && !c.ends_with(':') && *c != "." && *c != "..")
        .collect();
    for start in 0..parts.len() {
        let candidate = parts[start..].iter().fold(project_dir.to_path_buf(), |acc, c| acc.join(c));
        if exists(&candidate) {
            return candidate;
        }
    }
    p.to_path_buf()
}
/// Collects media item ids reachable from a sequence, following nested sequences (cycle-safe).
fn collect_media(
    seq_id: u64,
    sequences: &BTreeMap<u64, (&str, RawSequence)>,
    media: &BTreeMap<u64, (PathBuf, bool)>,
    out: &mut HashSet<u64>,
    seen: &mut HashSet<u64>,
) {
    if !seen.insert(seq_id) {
        return;
    }
    let Some((_, seq)) = sequences.get(&seq_id) else { return };
    for clip in seq.video_tracks.iter().chain(&seq.audio_tracks).flat_map(|t| &t.items) {
        if media.contains_key(&clip.item) {
            out.insert(clip.item);
        } else if sequences.contains_key(&clip.item) {
            collect_media(clip.item, sequences, media, out, seen);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: i64 = TICKS_PER_SECOND;

    /// Hand-built to the documented layout; replace with a real `.fcproj` fixture when available.
    fn sample() -> String {
        format!(
            r#"{{
          "format": "filmcraft.project", "schema_version": 17, "generator": "FilmCraft 0.12.0",
          "view": {{ "active_sequence": 3 }},
          "project": {{ "name": "Short Film", "next_id": 9, "root": {{}},
            "items": {{
              "1": {{ "id": 1, "name": "a.mov", "kind": {{ "Media": {{ "media": {{ "File": {{ "path": "media/a.mov" }} }}, "offline": false }} }} }},
              "2": {{ "id": 2, "name": "b.mov", "kind": {{ "Media": {{ "media": {{ "File": {{ "path": "/abs/b.mov" }} }}, "offline": true }} }} }},
              "3": {{ "id": 3, "name": "Final Cut", "kind": {{ "Sequence": {{
                  "settings": {{ "width": 1920, "height": 1080, "frame_rate": {{ "num": 24000, "den": 1001 }}, "par": [1,1] }},
                  "video_tracks": [ {{ "items": [
                      {{ "item": 1, "start": 0, "duration": {t10}, "source_in": 0 }},
                      {{ "item": 4, "start": {t10}, "duration": {t5}, "source_in": 0 }} ] }} ],
                  "audio_tracks": [ {{ "items": [ {{ "item": 2, "start": 0, "duration": {t12} }} ] }} ] }} }} }},
              "4": {{ "id": 4, "name": "Nested", "kind": {{ "Sequence": {{
                  "settings": {{ "width": 1280, "height": 720, "frame_rate": {{ "num": 30, "den": 1 }} }},
                  "video_tracks": [ {{ "items": [ {{ "item": 1, "start": 0, "duration": {t5} }} ] }} ], "audio_tracks": [] }} }} }},
              "5": {{ "id": 5, "name": "Title", "kind": {{ "Graphic": {{ "width": 1920, "height": 1080 }} }} }}
            }} }} }}"#,
            t10 = 10 * T,
            t5 = 5 * T,
            t12 = 12 * T
        )
    }

    fn parse(json: &str, exists: bool) -> Result<ProjectSummary, ImportError> {
        parse_project(json.as_bytes(), Path::new("/p/film.fcproj"), Path::new("/p"), &|_| exists)
    }

    #[test]
    fn lists_sequences_with_specs() {
        let s = parse(&sample(), true).unwrap();
        assert_eq!((s.name.as_str(), s.schema_version), ("Short Film", 17));
        assert_eq!(s.generator.as_deref(), Some("FilmCraft 0.12.0"));
        assert_eq!(s.sequences.len(), 2);

        let main = &s.sequences[0];
        assert_eq!(main.name, "Final Cut");
        assert!(!main.nested_in_other);
        assert_eq!((main.width, main.height), (1920, 1080));
        assert!((main.fps - 23.976).abs() < 0.001);
        assert_eq!(main.duration_secs, 15.0); // video ends at 15s, audio at 12s
        assert_eq!((main.video_tracks, main.audio_tracks, main.clips), (1, 1, 3));

        let nested = &s.sequences[1];
        assert!(nested.nested_in_other);
        assert_eq!(nested.fps, 30.0);
    }

    #[test]
    fn media_counts_follow_nesting_and_offline() {
        let s = parse(&sample(), true).unwrap();
        // Final Cut uses media 1, 2 and (through Nested) 1 again => 2 distinct; media 2 is offline.
        assert_eq!(s.sequences[0].media, MediaCounts { total: 2, missing: 1 });
        // Files not on disk count as missing too.
        let s = parse(&sample(), false).unwrap();
        assert_eq!(s.sequences[0].media.missing, 2);
    }

    #[test]
    fn relinks_media_when_the_project_moved_drives() {
        let json = r#"{"format":"filmcraft.project","schema_version":15,"project":{"name":"m","items":{
          "1":{"name":"a","kind":{"Media":{"media":{"File":{"path":"Q:\\Film\\Assets\\a.mov"}}}}},
          "2":{"name":"b","kind":{"Media":{"media":{"File":{"path":"Q:\\Film\\Smooth\\gone.mov"}}}}},
          "3":{"name":"S","kind":{"Sequence":{"video_tracks":[{"items":[
              {"item":1,"start":0,"duration":10},{"item":2,"start":10,"duration":10}]}]}}}}}}"#;
        let only_assets = |p: &Path| p == Path::new("/new/Film/Assets/a.mov").to_path_buf() || p == Path::new("/new/Assets/a.mov");
        let s = parse_project(json.as_bytes(), Path::new("/new/Film/x.fcproj"), Path::new("/new/Film"), &only_assets).unwrap();
        // a.mov is found under the project folder (Q:\Film\Assets\a.mov -> /new/Film/Assets/a.mov); gone.mov is not.
        assert_eq!(s.sequences[0].media, MediaCounts { total: 2, missing: 1 });
    }

    #[test]
    fn rejects_other_formats_and_garbage() {
        assert!(matches!(parse(r#"{"format":"premiere"}"#, true), Err(ImportError::WrongFormat(_))));
        assert!(matches!(parse("{}", true), Err(ImportError::WrongFormat(_))));
        assert!(matches!(parse("not json", true), Err(ImportError::Json(_))));
        assert!(matches!(
            parse(r#"{"format":"filmcraft.project","schema_version":17,"project":{"name":"x"}}"#, true),
            Err(ImportError::NoItems)
        ));
    }

    #[test]
    fn newer_schema_warns_but_reads() {
        let json = sample().replace("\"schema_version\": 17", "\"schema_version\": 40");
        let s = parse(&json, true).unwrap();
        assert_eq!(s.sequences.len(), 2);
        assert!(s.warnings.iter().any(|w| w.contains("schema 40")));
    }

    #[test]
    fn survives_cyclic_nesting_and_unknown_items() {
        let json = r#"{"format":"filmcraft.project","schema_version":17,"project":{"name":"c","items":{
          "1":{"name":"A","kind":{"Sequence":{"video_tracks":[{"items":[{"item":2,"start":0,"duration":10}]}]}}},
          "2":{"name":"B","kind":{"Sequence":{"video_tracks":[{"items":[{"item":1,"start":0,"duration":10}]}]}}},
          "3":{"name":"?","kind":{"FutureKind":{"x":1}}},
          "x":{"name":"bad id","kind":"Weird"}}}}"#;
        let s = parse(json, true).unwrap();
        assert_eq!(s.sequences.len(), 2);
    }
}
