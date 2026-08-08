//! Turning a parsed hunk into rows a diff view can lay out.
//!
//! Side-by-side and unified are two renderings of the same `Vec<DiffLine>`, so
//! the pairing logic lives here — outside either renderer — and is unit tested
//! without a window.
