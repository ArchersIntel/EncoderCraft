//! Pairs sequences with the master files exported from FilmCraft, by name.

use std::collections::HashSet;

use serde::Serialize;

const VIDEO_EXTS: &[&str] = &["mov", "mp4", "m4v", "mxf", "mkv", "avi", "mpg", "mpeg", "mts", "m2ts", "webm", "mj2"];
const MIN_PARTIAL_LEN: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// File name (without extension) equals the sequence name.
    Exact,
    /// File name contains the sequence name (e.g. `Final Cut_v2.mov`); worth a glance.
    Partial,
    /// Not matched by name, but a file with the same frame size, rate and length exists.
    Likely,
    /// Several files fit; the user must choose.
    Ambiguous,
    None,
}

#[derive(Debug, Clone, Serialize)]
pub struct MasterMatch {
    pub sequence_id: u64,
    /// The chosen file name when the match is unambiguous.
    pub file: Option<String>,
    pub kind: MatchKind,
    pub candidates: Vec<String>,
}

/// True for file names with a video container extension.
pub fn is_video(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| VIDEO_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
}

fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(s, _)| s)
}

/// Lowercase alphanumerics only, so "Final Cut", "final_cut" and "FINAL-CUT" compare equal.
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// `files` are bare file names from the folder the user pointed at.
pub fn match_masters(sequences: &[(u64, String)], files: &[String]) -> Vec<MasterMatch> {
    let videos: Vec<(&String, String)> =
        files.iter().filter(|f| is_video(f)).map(|f| (f, norm(stem(f)))).collect();
    let names: Vec<String> = sequences.iter().map(|(_, n)| norm(n)).collect();

    // Pass 1: exact matches; a file claimed exactly can't also be a partial match elsewhere.
    let exact: Vec<Vec<String>> = names
        .iter()
        .map(|n| videos.iter().filter(|(_, s)| !n.is_empty() && s == n).map(|(f, _)| (*f).clone()).collect())
        .collect();
    let claimed: HashSet<&String> = exact.iter().flatten().collect();

    sequences
        .iter()
        .zip(&names)
        .zip(exact.iter())
        .map(|(((id, _), name), exact)| {
            if !exact.is_empty() {
                return finish(*id, exact.clone(), MatchKind::Exact);
            }
            let partial: Vec<String> = if name.len() >= MIN_PARTIAL_LEN {
                videos
                    .iter()
                    .filter(|(f, s)| !claimed.contains(*f) && s.contains(name.as_str()))
                    .map(|(f, _)| (*f).clone())
                    .collect()
            } else {
                Vec::new()
            };
            finish(*id, partial, MatchKind::Partial)
        })
        .collect()
}

fn finish(sequence_id: u64, mut candidates: Vec<String>, kind_if_one: MatchKind) -> MasterMatch {
    candidates.sort();
    match candidates.len() {
        0 => MasterMatch { sequence_id, file: None, kind: MatchKind::None, candidates },
        1 => MasterMatch { sequence_id, file: Some(candidates[0].clone()), kind: kind_if_one, candidates },
        _ => MasterMatch { sequence_id, file: None, kind: MatchKind::Ambiguous, candidates },
    }
}

// ---- matching by media properties ---------------------------------------------------------

/// What a sequence should look like when exported.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SeqSpec {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_secs: f64,
}

/// What a candidate file actually is (from ffprobe).
#[derive(Debug, Clone)]
pub struct FileSpec {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Suggestion {
    pub sequence_id: u64,
    /// Files whose resolution, frame rate and length all fit the sequence.
    pub files: Vec<String>,
}

/// A container is a little longer than the last clip's end (audio tail, frame rounding), so allow
/// the larger of 0.15 s or three frames.
fn duration_tolerance(fps: f64) -> f64 {
    (3.0 / fps.max(1.0)).max(0.15)
}

/// Pairs sequences with files that have the same frame size and rate and a matching length.
/// Only sequences with at least one fitting file are returned.
pub fn suggest_by_specs(seqs: &[SeqSpec], files: &[FileSpec]) -> Vec<Suggestion> {
    seqs.iter()
        .filter_map(|s| {
            let mut fit: Vec<String> = files
                .iter()
                .filter(|f| {
                    f.width == s.width
                        && f.height == s.height
                        && (f.fps - s.fps).abs() < 0.01
                        && (f.duration_secs - s.duration_secs).abs() <= duration_tolerance(s.fps)
                })
                .map(|f| f.name.clone())
                .collect();
            fit.sort();
            (!fit.is_empty()).then_some(Suggestion { sequence_id: s.id, files: fit })
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    fn run(seqs: &[&str], files: &[&str]) -> Vec<MasterMatch> {
        let seqs: Vec<(u64, String)> = seqs.iter().enumerate().map(|(i, n)| (i as u64, (*n).into())).collect();
        match_masters(&seqs, &files.iter().map(|f| (*f).to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn exact_ignores_case_and_punctuation() {
        let m = run(&["Final Cut"], &["final_cut.MOV", "notes.txt"]);
        assert_eq!((m[0].kind, m[0].file.as_deref()), (MatchKind::Exact, Some("final_cut.MOV")));
    }

    #[test]
    fn partial_is_flagged_and_exact_wins_the_file() {
        let m = run(&["Final", "Final Cut"], &["Final Cut.mov", "Final_v2.mp4"]);
        // "Final Cut.mov" is exactly sequence 1's, so sequence 0 only gets Final_v2.mp4.
        assert_eq!((m[0].kind, m[0].file.as_deref()), (MatchKind::Partial, Some("Final_v2.mp4")));
        assert_eq!((m[1].kind, m[1].file.as_deref()), (MatchKind::Exact, Some("Final Cut.mov")));
    }

    #[test]
    fn several_candidates_are_ambiguous_and_none_is_none() {
        let m = run(&["Teaser", "Credits"], &["Teaser_h264.mp4", "Teaser_prores.mov"]);
        assert_eq!(m[0].kind, MatchKind::Ambiguous);
        assert_eq!(m[0].candidates.len(), 2);
        assert!(m[0].file.is_none());
        assert_eq!(m[1].kind, MatchKind::None);
    }

    fn seq(id: u64, w: u32, h: u32, fps: f64, d: f64) -> SeqSpec {
        SeqSpec { id, width: w, height: h, fps, duration_secs: d }
    }
    fn file(n: &str, w: u32, h: u32, fps: f64, d: f64) -> FileSpec {
        FileSpec { name: n.into(), width: w, height: h, fps, duration_secs: d }
    }

    #[test]
    fn suggests_by_specs_with_real_world_numbers() {
        // Numbers from a real project: containers run ~0.005-0.02 s longer than the clip end.
        let seqs = [seq(10, 2704, 1520, 119.88, 29.9966), seq(33, 1920, 1080, 29.97, 21.7551), seq(79, 1920, 1080, 23.976, 76.6599)];
        let files = [
            file("Lulz.mp4", 2704, 1520, 119.88, 30.016),
            file("Lulz2.mp4", 1920, 1080, 29.97, 21.760),
            file("Spooky.mp4", 1920, 1080, 23.976, 76.672),
        ];
        let s = suggest_by_specs(&seqs, &files);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], Suggestion { sequence_id: 10, files: vec!["Lulz.mp4".into()] });
        assert_eq!(s[1].files, vec!["Lulz2.mp4".to_string()]);
    }

    #[test]
    fn rejects_wrong_size_rate_or_length() {
        let seqs = [seq(1, 1920, 1080, 24.0, 60.0)];
        for f in [
            file("size.mp4", 1280, 720, 24.0, 60.0),
            file("rate.mp4", 1920, 1080, 25.0, 60.0),
            file("long.mp4", 1920, 1080, 24.0, 61.0),
            file("short.mp4", 1920, 1080, 24.0, 58.0),
        ] {
            assert!(suggest_by_specs(&seqs, &[f]).is_empty());
        }
        // An identical-spec pair of files is reported together so the user can choose.
        let both = [file("a.mp4", 1920, 1080, 24.0, 60.1), file("b.mp4", 1920, 1080, 24.0, 59.95)];
        assert_eq!(suggest_by_specs(&seqs, &both)[0].files.len(), 2);
    }

    #[test]
    fn tiny_names_never_partial_match() {
        let m = run(&["A"], &["Trailer.mov"]);
        assert_eq!(m[0].kind, MatchKind::None);
    }
}
