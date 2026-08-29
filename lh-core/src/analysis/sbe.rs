use crate::model::{FRAMES_PER_SECTOR, StreamInfo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sbe {
    /// Length is a whole number of CD sectors.
    Aligned,
    /// Length is short of a sector boundary by this many frames.
    Misaligned { remainder_frames: u64 },
    /// Sector boundaries are a CD-audio concept; anything else has no boundary to miss.
    NotApplicable { reason: &'static str },
}

/// A CD sector holds 588 frames (44100 / 75). A file whose frame count is not a whole
/// multiple of that cannot be burned gapless against its neighbours.
///
/// Only meaningful for 44.1 kHz / 16-bit / stereo. Anything else reports
/// [`Sbe::NotApplicable`] — never a pass, because "pass" would imply we checked.
pub fn sbe(info: &StreamInfo) -> Sbe {
    if !info.is_cdda() {
        return Sbe::NotApplicable {
            reason: "not CD audio (44.1 kHz / 16-bit / stereo)",
        };
    }
    let Some(frames) = info.total_frames else {
        return Sbe::NotApplicable {
            reason: "frame count unknown",
        };
    };
    match frames % FRAMES_PER_SECTOR {
        0 => Sbe::Aligned,
        remainder_frames => Sbe::Misaligned { remainder_frames },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cdda(frames: u64) -> StreamInfo {
        StreamInfo {
            sample_rate: 44_100,
            channels: 2,
            bits_per_sample: 16,
            total_frames: Some(frames),
            audio_md5: None,
        }
    }

    #[test]
    fn whole_sectors_are_aligned() {
        assert_eq!(sbe(&cdda(588 * 100)), Sbe::Aligned);
        assert_eq!(sbe(&cdda(0)), Sbe::Aligned);
    }

    #[test]
    fn short_of_a_boundary_is_misaligned() {
        assert_eq!(
            sbe(&cdda(588 * 100 + 1)),
            Sbe::Misaligned {
                remainder_frames: 1
            }
        );
        assert_eq!(
            sbe(&cdda(587)),
            Sbe::Misaligned {
                remainder_frames: 587
            }
        );
    }

    #[test]
    fn non_cdda_is_never_a_pass() {
        let mut info = cdda(1);
        info.sample_rate = 48_000;
        assert!(matches!(sbe(&info), Sbe::NotApplicable { .. }));

        let mut info = cdda(1);
        info.bits_per_sample = 24;
        assert!(matches!(sbe(&info), Sbe::NotApplicable { .. }));

        let mut info = cdda(1);
        info.channels = 1;
        assert!(matches!(sbe(&info), Sbe::NotApplicable { .. }));
    }

    #[test]
    fn unknown_length_is_not_applicable() {
        let mut info = cdda(0);
        info.total_frames = None;
        assert!(matches!(sbe(&info), Sbe::NotApplicable { .. }));
    }
}
