//! Audio in memory and on disk.
//!
//! One shape, everywhere: 16-bit mono PCM. Capture produces it, a WAV file
//! carries it, and every transcription backend is handed the same thing, so a
//! model can be swapped without touching the recorder.
//!
//! Whisper's models are trained on 16 kHz mono, so that is the only rate the
//! capture path produces. Reading a file accepts other rates but refuses to
//! guess at resampling: a wrong rate silently produces nonsense, so it is an
//! error with the `ffmpeg` command that fixes it.

use std::io::Write;
use std::path::Path;

use crate::error::CoreError;

/// The sample rate every transcription backend expects.
pub const WHISPER_RATE: u32 = 16_000;
/// Bytes per sample in the PCM we carry around.
pub const BYTES_PER_SAMPLE: usize = 2;

/// Mono 16-bit PCM, held in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audio {
    rate: u32,
    samples: Vec<i16>,
}

impl Audio {
    /// Wrap samples that are already mono at `rate`.
    pub fn new(rate: u32, samples: Vec<i16>) -> Self {
        Self { rate, samples }
    }

    /// Interpret little-endian 16-bit mono bytes, as a capture program writes
    /// them to stdout.
    pub fn from_s16le(rate: u32, bytes: &[u8]) -> Self {
        let samples = bytes
            .chunks_exact(BYTES_PER_SAMPLE)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        Self { rate, samples }
    }

    /// The samples as little-endian 16-bit mono bytes.
    pub fn to_s16le(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.samples.len() * 2);
        for sample in &self.samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Length in seconds, the unit users and logs talk about.
    pub fn seconds(&self) -> f64 {
        self.samples.len() as f64 / f64::from(self.rate)
    }

    /// Root-mean-square amplitude, 0.0 to 1.0. Used to tell "nothing was
    /// captured" from "the model produced nothing".
    pub fn rms(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let sum: f64 = self
            .samples
            .iter()
            .map(|sample| {
                let value = f64::from(*sample) / f64::from(i16::MAX);
                value * value
            })
            .sum();
        (sum / self.samples.len() as f64).sqrt()
    }

    /// Cut [start, end) seconds out, clamped to what is there.
    pub fn slice_seconds(&self, start: f64, end: f64) -> Self {
        let from = (start * f64::from(self.rate)).max(0.0) as usize;
        let to = ((end * f64::from(self.rate)) as usize).min(self.samples.len());
        let to = to.max(from.min(self.samples.len()));
        Self {
            rate: self.rate,
            samples: self.samples.get(from..to).unwrap_or_default().to_vec(),
        }
    }
}

/// Serialise as a 16-bit PCM WAV file.
pub fn to_wav_bytes(audio: &Audio) -> Vec<u8> {
    let data_len = (audio.len() * BYTES_PER_SAMPLE) as u32;
    let channels = 1u16;
    let bits = (BYTES_PER_SAMPLE * 8) as u16;
    let block_align = channels * (bits / 8);
    let byte_rate = audio.rate() * u32::from(block_align);

    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&audio.rate().to_le_bytes());
    bytes.extend_from_slice(&byte_rate.to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&bits.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    bytes.extend_from_slice(&audio.to_s16le());
    bytes
}

/// Write `audio` to `path` as a WAV file, creating parent directories.
pub fn write_wav(path: &Path, audio: &Audio) -> Result<(), CoreError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(&to_wav_bytes(audio))?;
    file.flush()?;
    Ok(())
}

/// Read a 16-bit PCM WAV file, mixing any number of channels down to mono.
pub fn read_wav(path: &Path) -> Result<Audio, CoreError> {
    let bytes = std::fs::read(path).map_err(|error| {
        CoreError::Voice(format!(
            "cannot read the audio file `{}`: {error}",
            path.display()
        ))
    })?;
    parse_wav(&bytes).map_err(|message| {
        CoreError::Voice(format!(
            "`{}` is not usable audio: {message}",
            path.display()
        ))
    })
}

/// Parse WAV bytes: PCM only, any channel count, any sample rate.
fn parse_wav(bytes: &[u8]) -> Result<Audio, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".to_string());
    }

    let mut offset = 12;
    let mut format: Option<(u16, u16, u32, u16)> = None; // code, channels, rate, bits
    let mut data: Option<&[u8]> = None;

    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let length = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        let body_start = offset + 8;
        let body_end = body_start.saturating_add(length).min(bytes.len());
        let body = &bytes[body_start..body_end];

        match id {
            b"fmt " => {
                if body.len() < 16 {
                    return Err("the `fmt ` chunk is truncated".to_string());
                }
                let code = u16::from_le_bytes([body[0], body[1]]);
                let channels = u16::from_le_bytes([body[2], body[3]]);
                let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
                let bits = u16::from_le_bytes([body[14], body[15]]);
                format = Some((code, channels, rate, bits));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks are word aligned: an odd length is followed by a pad byte.
        offset = body_start + length + (length % 2);
    }

    let (code, channels, rate, bits) =
        format.ok_or_else(|| "no `fmt ` chunk (is this a WAV file?)".to_string())?;
    let data = data.ok_or_else(|| "no `data` chunk (the file holds no audio)".to_string())?;

    if code != 1 {
        return Err(format!(
            "sample format {code} is not PCM (only 16-bit PCM WAV is supported)"
        ));
    }
    if bits != 16 {
        return Err(format!(
            "{bits}-bit samples are not supported (only 16-bit PCM)"
        ));
    }
    if channels == 0 {
        return Err("the file declares zero channels".to_string());
    }
    if rate == 0 {
        return Err("the file declares a sample rate of zero".to_string());
    }
    if rate != WHISPER_RATE {
        return Err(format!(
            "{rate} Hz is not {WHISPER_RATE} Hz; models are trained on {WHISPER_RATE} Hz mono, \
             convert first: ffmpeg -i in.wav -ar {WHISPER_RATE} -ac 1 out.wav"
        ));
    }

    let frame_bytes = usize::from(channels) * BYTES_PER_SAMPLE;
    let frames = data.len() / frame_bytes;
    let mut samples = Vec::with_capacity(frames);
    for frame in data.chunks_exact(frame_bytes).take(frames) {
        // Average the channels: cheap, and right for the stereo cases a user
        // hits in practice (one channel silent, or the same signal twice).
        let mut sum = 0i32;
        for channel in 0..usize::from(channels) {
            let at = channel * BYTES_PER_SAMPLE;
            sum += i32::from(i16::from_le_bytes([frame[at], frame[at + 1]]));
        }
        samples.push((sum / i32::from(channels)) as i16);
    }

    Ok(Audio { rate, samples })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize) -> Audio {
        let samples = (0..frames)
            .map(|index| ((index % 100) as i16 - 50) * 200)
            .collect();
        Audio::new(WHISPER_RATE, samples)
    }

    #[test]
    fn s16le_bytes_round_trip() {
        let audio = tone(1500);
        let bytes = audio.to_s16le();
        assert_eq!(bytes.len(), audio.len() * BYTES_PER_SAMPLE);
        assert_eq!(Audio::from_s16le(WHISPER_RATE, &bytes), audio);
    }

    #[test]
    fn a_trailing_odd_byte_is_ignored() {
        let audio = Audio::from_s16le(WHISPER_RATE, &[0x01, 0x00, 0x02]);
        assert_eq!(audio.samples(), [1]);
    }

    #[test]
    fn a_wav_file_round_trips_through_disk() {
        let directory = std::env::temp_dir().join(format!("blb-audio-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("tone.wav");
        let audio = tone(3200);

        write_wav(&path, &audio).unwrap();
        let read = read_wav(&path).unwrap();

        assert_eq!(read, audio);
        assert!((read.seconds() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn the_header_is_a_standard_44_byte_pcm_header() {
        let bytes = to_wav_bytes(&tone(4));
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");
        // 36 + data length, and the data length itself.
        assert_eq!(
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            36 + 8
        );
        assert_eq!(
            u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]),
            8
        );
        assert_eq!(bytes.len(), 44 + 8);
    }

    #[test]
    fn stereo_is_mixed_down_to_mono() {
        let mut bytes = to_wav_bytes(&Audio::new(WHISPER_RATE, vec![0, 0]));
        // Rewrite the fmt chunk as 2 channels and rebuild the data chunk.
        bytes[22] = 2; // channels
        let data = [100u8, 0, 0, 0, 200, 0, 200, 0];
        bytes.truncate(44);
        bytes[4..8].copy_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        bytes[40..44].copy_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);

        let audio = parse_wav(&bytes).unwrap();
        assert_eq!(audio.samples(), [50, 200], "averaged, not truncated");
    }

    #[test]
    fn junk_is_rejected_with_a_reason() {
        let error = parse_wav(b"nope").unwrap_err();
        assert!(error.contains("RIFF"), "{error}");

        let mut bytes = to_wav_bytes(&tone(10));
        bytes[20..22].copy_from_slice(&3u16.to_le_bytes()); // 3 = float samples
        let error = parse_wav(&bytes).unwrap_err();
        assert!(error.contains("PCM"), "{error}");
    }

    #[test]
    fn the_wrong_sample_rate_says_how_to_fix_it() {
        let mut bytes = to_wav_bytes(&tone(10));
        bytes[24..28].copy_from_slice(&44_100u32.to_le_bytes());
        let error = parse_wav(&bytes).unwrap_err();
        assert!(error.contains("44100 Hz"), "{error}");
        assert!(error.contains("ffmpeg"), "{error}");
    }

    #[test]
    fn silence_has_no_energy_and_a_tone_does() {
        let silent = Audio::new(WHISPER_RATE, vec![0; 160]);
        assert_eq!(silent.rms(), 0.0);

        let loud = Audio::new(WHISPER_RATE, vec![i16::MAX; 160]);
        assert!((loud.rms() - 1.0).abs() < 1e-6);
        assert!(tone(1600).rms() > 0.01);
    }

    #[test]
    fn slices_are_clamped_to_the_audio_that_exists() {
        let audio = tone(16_000);
        assert_eq!(audio.slice_seconds(0.0, 0.5).len(), 8000);
        assert_eq!(audio.slice_seconds(0.5, 0.5).len(), 0);
        assert_eq!(audio.slice_seconds(2.0, 3.0).len(), 0);
        assert_eq!(audio.slice_seconds(-1.0, 0.25).len(), 4000);
    }
}
