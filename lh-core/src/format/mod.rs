pub mod flac;
pub mod wav;

use crate::error::{Error, Result};
use crate::model::{AudioFile, AudioFormat};
use std::path::Path;

/// Read everything we can learn about a file without decoding it.
pub fn probe(path: &Path) -> Result<AudioFile> {
    let format = AudioFormat::from_path(path).ok_or_else(|| Error::UnknownFormat {
        path: path.to_path_buf(),
    })?;

    if !format.is_implemented() {
        // Principle 5: name the format and the tool, never a generic failure.
        return Err(Error::Unsupported {
            path: path.to_path_buf(),
            format: format.name(),
            tool: format.required_tool(),
        });
    }

    let file_size = std::fs::metadata(path)
        .map_err(|e| Error::io(path, e))?
        .len();

    let (stream_info, encoder) = match format {
        AudioFormat::Flac => {
            let (si, enc) = flac::probe(path)?;
            (si, enc)
        }
        AudioFormat::Wav => (wav::probe(path)?.stream_info, None),
        _ => unreachable!("guarded by is_implemented above"),
    };

    Ok(AudioFile {
        path: path.to_path_buf(),
        format,
        file_size,
        stream_info,
        encoder,
    })
}

/// MD5 of the unencoded audio, computed by actually decoding the file.
///
/// This is the value FLAC stores in STREAMINFO and the value shntool hashes for `.st5`,
/// so the two agree by construction — a property the test suite pins.
pub fn audio_md5(path: &Path) -> Result<[u8; 16]> {
    let format = AudioFormat::from_path(path).ok_or_else(|| Error::UnknownFormat {
        path: path.to_path_buf(),
    })?;
    match format {
        AudioFormat::Flac => flac::audio_md5(path),
        AudioFormat::Wav => wav::audio_md5(path),
        other => Err(Error::Unsupported {
            path: path.to_path_buf(),
            format: other.name(),
            tool: other.required_tool(),
        }),
    }
}
