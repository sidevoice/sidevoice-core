//! Rustvani's detector stays behind this application boundary.

mod detector;
mod probe;
mod speech_gate;

pub use detector::{CallDetector, CallFrame};
pub use probe::{probe_detectors, DetectorReadout};
pub use speech_gate::has_speech;
