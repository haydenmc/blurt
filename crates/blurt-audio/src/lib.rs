//! Real-time audio engine for blurt.
//!
//! Capture → resample → echo cancellation/gain (sonora) → noise suppression
//! (nnnoiseless) → VAD/PTT gate → Opus encode, and per-speaker jitter buffer →
//! Opus decode → mix → playback. Implemented in milestone 3.
