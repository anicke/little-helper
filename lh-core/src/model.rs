use std::fmt;
use std::path::{Path, PathBuf};

/// Frames per CD sector: 44100 / 75.
pub const FRAMES_PER_SECTOR: u64 = 588;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Flac,
    Wav,
    /// Recognized by extension but not implemented; carries the tool that would be needed.
    Shn,
    Ape,
    Wv,
    Tta,
}

impl AudioFormat {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "flac" | "fla" => Self::Flac,
            "wav" | "wave" => Self::Wav,
            "shn" => Self::Shn,
            "ape" => Self::Ape,
            "wv" => Self::Wv,
            "tta" => Self::Tta,
            _ => return None,
        })
    }

    pub fn is_implemented(self) -> bool {
        matches!(self, Self::Flac | Self::Wav)
    }

    /// The external tool that would be required to handle this format.
    pub fn required_tool(self) -> &'static str {
        match self {
            Self::Flac | Self::Wav => "nothing",
            Self::Shn => "shntool",
            Self::Ape => "mac",
            Self::Wv => "wvunpack",
            Self::Tta => "ttaenc",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Flac => "FLAC",
            Self::Wav => "WAV",
            Self::Shn => "SHN",
            Self::Ape => "APE",
            Self::Wv => "WavPack",
            Self::Tta => "TTA",
        }
    }
}

impl fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
    /// Frames (sample points per channel). `None` when the container does not state it.
    pub total_frames: Option<u64>,
    /// MD5 of the unencoded audio, as stored in FLAC STREAMINFO. All-zero means "not set".
    pub audio_md5: Option<[u8; 16]>,
}

impl StreamInfo {
    /// CD audio: 44.1 kHz, 16-bit, stereo. Sector boundaries are only meaningful here.
    pub fn is_cdda(&self) -> bool {
        self.sample_rate == 44_100 && self.channels == 2 && self.bits_per_sample == 16
    }

    pub fn duration_secs(&self) -> Option<f64> {
        self.total_frames
            .map(|f| f as f64 / self.sample_rate as f64)
    }

    pub fn bytes_per_frame(&self) -> u32 {
        u32::from(self.channels) * u32::from(self.bits_per_sample).div_ceil(8)
    }
}

#[derive(Debug, Clone)]
pub struct AudioFile {
    pub path: PathBuf,
    pub format: AudioFormat,
    pub file_size: u64,
    pub stream_info: StreamInfo,
    /// FLAC vendor string, e.g. "reference libFLAC 1.5.0 20250211". The provenance record
    /// other traders inspect (Principle 2).
    pub encoder: Option<String>,
}

impl AudioFile {
    /// The file's name as it appears in checksum files.
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}
