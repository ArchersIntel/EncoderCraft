//! Readers for ArtCraft project files. Currently: FilmCraft `.fcproj`.
//!
//! The encoder never renders a project. It reads one to list its sequences, then pairs each
//! sequence with a master the user exported from FilmCraft (see [`masters`]).

pub mod fcproj;
pub mod masters;

pub use fcproj::{read_project, ImportError, MediaCounts, ProjectSummary, SequenceInfo};
pub use masters::{is_video, match_masters, suggest_by_specs, FileSpec, MasterMatch, MatchKind, SeqSpec, Suggestion};
