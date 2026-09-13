//! Thin opt-in adapter: all parsing, history and playlists use the production index.
use super::Index;
use std::fs::File;

#[derive(Default)]
pub struct FuzzIndex(Index);

#[derive(Debug, PartialEq)]
pub struct Snapshot {
    pub fragments: Vec<(u64, u64, f64)>,
    pub segments: Vec<(u64, u64, f64)>,
    pub duration: Option<f64>,
    pub native: Result<String, String>,
    pub mse: Result<String, String>,
}

impl FuzzIndex {
    /// Start an independent raw-input case without trusting prior validation.
    pub fn begin_case() -> Self {
        super::completed::reset_for_fuzz_input();
        Self::default()
    }

    pub fn update(&mut self, file: &File, complete: bool) -> Result<(), String> {
        self.0.update_file(file, complete)
    }

    pub fn snapshot(&mut self) -> Snapshot {
        let index = &mut self.0;
        Snapshot {
            fragments: index
                .fragments
                .view(0, index.fragments.len())
                .iter()
                .map(|s| (s.offset, s.length, s.duration))
                .collect(),
            segments: index
                .segments
                .view(0, index.segments.len())
                .iter()
                .map(|s| (s.offset, s.length, s.duration))
                .collect(),
            duration: index.produced_duration_seconds(),
            native: index
                .playlist_view_for(false, None)
                .and_then(|v| v.render("/init", "/segment")),
            mse: index
                .mse_playlist_view(0)
                .and_then(|v| v.render("/init", "/fragment")),
        }
    }
}
