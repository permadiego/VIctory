//! Accessibility: speech synthesis for the title's narration, spoken by
//! the host's WinRT speech synthesizer in the selected Windows voice. Streams are
//! WAVE files of 16 kHz mono 16-bit PCM, the console's format; with no
//! voice installed (or off Windows) a stream is valid WAVE audio of no
//! length and the narration is silent, as before.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};

use super::xasync::{E_INVALIDARG, E_NOT_SUFFICIENT_BUFFER};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, text,
};

/// Closed captions, high contrast, speech to text and synthesis.
pub const PROVIDER: Guid = Guid::new(
    0x3e24_1977,
    0x6237,
    0x41e9,
    [0x85, 0x59, 0x10, 0x2c, 0x2d, 0x99, 0x83, 0xf1],
);
/// Unchanged since 2020.
pub const IID: Guid = Guid::new(
    0xd722_b373,
    0x8c4d,
    0x4692,
    [0x9e, 0x51, 0xd6, 0xad, 0x9b, 0x37, 0xaa, 0x7d],
);

pub static OBJECT: Object = Object::new("accessibility", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID, &OBJECT)];

/// The console's stream format: 16 kHz mono 16-bit PCM.
const RATE: u32 = 16_000;

mod voices;
use voices::{close, create, enumerate_voices, set_custom_voice, set_default_voice};

type This = *const Object;

/// A WAVE file of [`RATE`] mono 16-bit PCM holding `samples`: RIFF, a
/// 16-byte format chunk, the data chunk.
fn wave(samples: &[i16]) -> Vec<u8> {
    let samples = &samples[..samples.len().min(u32::MAX as usize / 2 - 44)];
    let data = (samples.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes()); // block
    w.extend_from_slice(&16u16.to_le_bytes()); // bits
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

/// The samples of a 16-bit PCM WAVE file mixed to mono and resampled
/// (linearly) to [`RATE`]; `None` for anything else.
fn to_console(file: &[u8]) -> Option<Vec<i16>> {
    if file.get(..4)? != b"RIFF" || file.get(8..12)? != b"WAVE" {
        return None;
    }
    let (mut fmt, mut data) = (None, None);
    let mut at = 12usize;
    while at.checked_add(8)? <= file.len() {
        let len = u32::from_le_bytes(file[at + 4..at + 8].try_into().ok()?) as usize;
        let end = (at + 8).saturating_add(len).min(file.len());
        match &file[at..at + 4] {
            b"fmt " => fmt = Some(&file[at + 8..end]),
            b"data" => data = Some(&file[at + 8..end]),
            _ => {}
        }
        at = at
            .saturating_add(8)
            .saturating_add(len)
            .saturating_add(len & 1);
    }
    let fmt = fmt?;
    let u16_at = |k: usize| fmt.get(k..k + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let (tag, channels, bits) = (u16_at(0)?, usize::from(u16_at(2)?), u16_at(14)?);
    let rate = u32::from_le_bytes(fmt.get(4..8)?.try_into().ok()?);
    // PCM, or WAVE_FORMAT_EXTENSIBLE around it.
    if !matches!(tag, 1 | 0xFFFE) || bits != 16 || channels == 0 || rate == 0 {
        return None;
    }
    let mono: Vec<f32> = data?
        .chunks_exact(2 * channels)
        .map(|frame| {
            let sum: f32 = frame
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| f32::from(i16::from_le_bytes(*s)))
                .sum();
            sum / channels as f32
        })
        .collect();
    let Some(last) = mono.len().checked_sub(1) else {
        return Some(Vec::new());
    };
    let n = (mono.len() as u64 * u64::from(RATE) / u64::from(rate)) as usize;
    let step = f64::from(rate) / f64::from(RATE);
    Some(
        (0..n)
            .map(|i| {
                let x = i as f64 * step;
                let k = (x as usize).min(last);
                let t = (x - k as f64) as f32;
                let (a, b) = (mono[k], mono[(k + 1).min(last)]);
                (a + (b - a) * t).round().clamp(-32768.0, 32767.0) as i16
            })
            .collect(),
    )
}

/// SSML's text: the markup between `<` and `>` dropped.
fn plain(ssml: &str) -> String {
    let mut out = String::with_capacity(ssml.len());
    let mut tag = false;
    for c in ssml.chars() {
        match c {
            '<' => tag = true,
            '>' if tag => {
                tag = false;
                out.push(' ');
            }
            c if !tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// The host's speech for `words` (SSML with `ssml`) as a WAVE file in the
/// host synthesizer's format; `None` without a voice or on failure.
#[cfg(windows)]
fn speak(words: &str, ssml: bool, voice: Option<&str>) -> Option<Vec<u8>> {
    let words = windows::core::HSTRING::from(words);
    let voice = voice.map(windows::core::HSTRING::from);
    // A thread of our own in the multithreaded apartment, whatever the
    // title's thread initialised.
    let spoken = std::thread::spawn(move || -> windows::core::Result<Vec<u8>> {
        use windows::Media::SpeechSynthesis::SpeechSynthesizer;
        use windows::Storage::Streams::DataReader;
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        // SAFETY: initialises COM on this thread, which ends after the call.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let synth = SpeechSynthesizer::new()?;
        if let Some(id) = voice {
            synth.SetVoice(&voices::find(&id)?)?;
        }
        let stream = if ssml {
            synth.SynthesizeSsmlToStreamAsync(&words)?.join()?
        } else {
            synth.SynthesizeTextToStreamAsync(&words)?.join()?
        };
        let size = u32::try_from(stream.Size()?).unwrap_or(u32::MAX);
        let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
        reader.LoadAsync(size)?.join()?;
        let mut bytes = vec![0; reader.UnconsumedBufferLength()? as usize];
        reader.ReadBytes(&mut bytes)?;
        Ok(bytes)
    })
    .join()
    .ok()?;
    spoken
        .inspect_err(|e| tracing::debug!(error = %e, ssml, "speech synthesis failed"))
        .ok()
}

#[cfg(not(windows))]
fn speak(_: &str, _: bool, _: Option<&str>) -> Option<Vec<u8>> {
    None
}

/// The narration of `words` at the console's format; SSML the host's
/// synthesizer refuses is spoken as its plain text.
fn narration(words: &str, ssml: bool, voice: Option<&str>) -> Vec<i16> {
    if words.trim().is_empty() {
        return Vec::new();
    }
    let host = speak(words, ssml, voice)
        .or_else(|| ssml.then(|| speak(&plain(words), false, voice)).flatten());
    host.and_then(|file| to_console(&file)).unwrap_or_default()
}

fn create_stream(
    handle: *const u8,
    words: *const c_char,
    ssml: bool,
    out: *mut *const u8,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let words = text(words).unwrap_or_default();
    let Some(voice) = voices::selected(handle) else {
        return E_INVALIDARG;
    };
    let samples = narration(&words, ssml, voice.as_deref());
    tracing::debug!(text = words, samples = samples.len(), "speech");
    let file = Box::into_raw(Box::new(wave(&samples)));
    put(out, file.cast_const().cast())
}
extern "system" fn stream_from_text(
    _: This,
    handle: *const u8,
    words: *const c_char,
    out: *mut *const u8,
) -> HRESULT {
    create_stream(handle, words, false, out)
}
extern "system" fn stream_from_ssml(
    _: This,
    handle: *const u8,
    ssml: *const c_char,
    out: *mut *const u8,
) -> HRESULT {
    create_stream(handle, ssml, true, out)
}

/// The WAVE file behind a stream handle.
fn file<'a>(stream: *const u8) -> Option<&'a [u8]> {
    // SAFETY: stream handles are the boxed files `create_stream` made,
    // alive until `close_stream`.
    (!stream.is_null()).then(|| unsafe { (*stream.cast::<Vec<u8>>()).as_slice() })
}
extern "system" fn close_stream(_: This, stream: *const u8) -> HRESULT {
    if !stream.is_null() {
        // SAFETY: as in `file`; the title closes a stream once.
        drop(unsafe { Box::from_raw(stream.cast::<Vec<u8>>().cast_mut()) });
    }
    S_OK
}
extern "system" fn stream_size(_: This, stream: *const u8, out: *mut usize) -> HRESULT {
    match file(stream) {
        Some(f) => put(out, f.len()),
        None => E_INVALIDARG,
    }
}
extern "system" fn stream_data(
    _: This,
    stream: *const u8,
    size: usize,
    buffer: *mut u8,
    used: *mut usize,
) -> HRESULT {
    let Some(f) = file(stream) else {
        return E_INVALIDARG;
    };
    if buffer.is_null() || size < f.len() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe { buffer.copy_from_nonoverlapping(f.as_ptr(), f.len()) };
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(f.len()) };
    }
    S_OK
}

static VTABLE: Methods<22> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    missing::<3> as *const (),      // 0x18 closed captions
    missing::<4> as *const (),      // 0x20
    missing::<5> as *const (),      // 0x28 high contrast
    missing::<6> as *const (),      // 0x30 speech to text
    missing::<7> as *const (),      // 0x38
    enumerate_voices as *const (),  // 0x40
    create as *const (),            // 0x48
    close as *const (),             // 0x50
    set_default_voice as *const (), // 0x58
    set_custom_voice as *const (),  // 0x60
    stream_from_text as *const (),  // 0x68 from text
    close_stream as *const (),      // 0x70 a stream
    stream_size as *const (),       // 0x78
    stream_data as *const (),       // 0x80
    missing::<17> as *const (),     // 0x88 speech to text
    missing::<18> as *const (),     // 0x90
    missing::<19> as *const (),     // 0x98
    missing::<20> as *const (),     // 0xa0
    stream_from_ssml as *const (),  // 0xa8 from SSML
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    type Create = extern "system" fn(This, *mut *const u8) -> HRESULT;
    type Stream = extern "system" fn(This, *const u8, *const c_char, *mut *const u8) -> HRESULT;
    type Close = extern "system" fn(This, *const u8) -> HRESULT;
    type Size = extern "system" fn(This, *const u8, *mut usize) -> HRESULT;
    type Data = extern "system" fn(This, *const u8, usize, *mut u8, *mut usize) -> HRESULT;

    /// The stream the title gets for `words` through `slot`, read as it
    /// reads one, then closed.
    fn spoken(slot: usize, words: &std::ffi::CStr) -> Vec<u8> {
        let this: This = &OBJECT;
        let (mut synth, mut stream, mut size) = (std::ptr::null(), std::ptr::null(), 0);
        assert_eq!(method::<Create>(&OBJECT, 0x48)(this, &mut synth), S_OK);
        let say = method::<Stream>(&OBJECT, slot);
        assert_eq!(say(this, synth, words.as_ptr(), &mut stream), S_OK);
        assert_eq!(method::<Size>(&OBJECT, 0x78)(this, stream, &mut size), S_OK);
        let mut wav = vec![0u8; size];
        let mut used = 0;
        let data = method::<Data>(&OBJECT, 0x80);
        let short = data(this, stream, size - 1, wav.as_mut_ptr(), &mut used);
        assert_eq!(short, E_NOT_SUFFICIENT_BUFFER);
        assert_eq!(data(this, stream, size, wav.as_mut_ptr(), &mut used), S_OK);
        assert_eq!(used, size);
        assert_eq!(method::<Close>(&OBJECT, 0x70)(this, stream), S_OK);
        assert_eq!(method::<Close>(&OBJECT, 0x50)(this, synth), S_OK);
        assert_eq!(
            (&wav[..4], &wav[8..16], &wav[36..40]),
            (&b"RIFF"[..], &b"WAVEfmt "[..], &b"data"[..])
        );
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), RATE);
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(data_len, size - 44);
        wav
    }

    #[test]
    fn empty_text_is_an_empty_wave_file() {
        assert_eq!(spoken(0x68, c"").len(), 44);
    }

    #[test]
    fn text_and_ssml_are_spoken_when_a_voice_is_installed() {
        if speak("a", false, None).is_none() {
            eprintln!("no voice; skipped");
            return;
        }
        let ssml = c"<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'>Press <emphasis>A</emphasis> to continue</speak>";
        for (slot, words) in [(0x68, c"Press A to continue"), (0xa8, ssml)] {
            let wav = spoken(slot, words);
            let loud = wav[44..]
                .as_chunks::<2>()
                .0
                .iter()
                .filter(|s| i16::from_le_bytes(**s).unsigned_abs() > 1000)
                .count();
            // Half a second or more of audio, and not silence.
            assert!(
                wav.len() > 44 + RATE as usize,
                "{slot:#x}: {} bytes",
                wav.len()
            );
            assert!(loud > 1000, "{slot:#x}: {loud} loud samples");
        }
    }

    #[test]
    fn host_audio_is_mixed_and_resampled() {
        // 22,050 Hz stereo: left 1000, right 3000, for 2,205 frames, after
        // an odd-sized chunk the reader skips with its pad byte.
        let mut file = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        for v in [16u32, 1 | 2 << 16, 22_050, 22_050 * 4, 4 | 16 << 16] {
            file.extend_from_slice(&v.to_le_bytes());
        }
        file.extend_from_slice(b"LIST");
        file.extend_from_slice(&3u32.to_le_bytes());
        file.extend_from_slice(b"abc\0data");
        file.extend_from_slice(&(2_205u32 * 4).to_le_bytes());
        for _ in 0..2_205 {
            file.extend_from_slice(&1000i16.to_le_bytes());
            file.extend_from_slice(&3000i16.to_le_bytes());
        }
        let out = to_console(&file).unwrap();
        assert_eq!(out.len(), 1_600);
        assert!(out.iter().all(|&s| s == 2000));
        // Not 16-bit PCM, or not WAVE.
        let mut eight = file.clone();
        eight[34] = 8;
        assert_eq!(to_console(&eight), None);
        assert_eq!(to_console(b"RIFF\0\0\0\0AVI "), None);
    }

    #[test]
    fn ssml_text_without_markup() {
        let words = plain("<speak>Press <emphasis>A</emphasis></speak>");
        assert_eq!(words.split_whitespace().collect::<Vec<_>>(), ["Press", "A"]);
    }
}
