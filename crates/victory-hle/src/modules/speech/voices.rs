use super::*;
use std::collections::HashMap;
use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

type Selections = HashMap<usize, Option<String>>;
static SELECTIONS: LazyLock<Mutex<Selections>> = LazyLock::new(Mutex::default);
static NEXT: AtomicUsize = AtomicUsize::new(1);

fn selections() -> MutexGuard<'static, Selections> {
    SELECTIONS.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn selected(handle: *const u8) -> Option<Option<String>> {
    selections().get(&(handle as usize)).cloned()
}

pub(super) extern "system" fn create(_: This, out: *mut *const u8) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    selections().insert(id, None);
    put(out, id as *const u8)
}

pub(super) extern "system" fn close(_: This, handle: *const u8) -> HRESULT {
    selections().remove(&(handle as usize));
    S_OK
}

fn choose(handle: *const u8, choice: Option<String>) -> HRESULT {
    let mut selections = selections();
    let Some(slot) = selections.get_mut(&(handle as usize)) else {
        return E_INVALIDARG;
    };
    *slot = choice;
    S_OK
}

pub(super) extern "system" fn set_default_voice(_: This, handle: *const u8) -> HRESULT {
    choose(handle, None)
}

pub(super) extern "system" fn set_custom_voice(
    _: This,
    handle: *const u8,
    id: *const c_char,
) -> HRESULT {
    let Some(id) = text(id) else {
        return E_INVALIDARG;
    };
    if !catalog().iter().any(|v| v.id.as_bytes() == id.as_bytes()) {
        return E_INVALIDARG;
    }
    choose(handle, Some(id))
}

struct Voice {
    description: CString,
    name: CString,
    gender: u32,
    id: CString,
    language: CString,
}

#[repr(C)]
pub(super) struct Info {
    description: *const c_char,
    name: *const c_char,
    gender: u32,
    id: *const c_char,
    language: *const c_char,
}

type Callback = extern "system" fn(*const Info, *mut c_void) -> bool;

pub(super) extern "system" fn enumerate_voices(
    _: This,
    context: *mut c_void,
    callback: Option<Callback>,
) -> HRESULT {
    let Some(callback) = callback else {
        return E_INVALIDARG;
    };
    for v in catalog() {
        let info = Info {
            description: v.description.as_ptr(),
            name: v.name.as_ptr(),
            gender: v.gender,
            id: v.id.as_ptr(),
            language: v.language.as_ptr(),
        };
        if !callback(&info, context) {
            break;
        }
    }
    S_OK
}

#[cfg(not(windows))]
fn catalog() -> Vec<Voice> {
    Vec::new()
}

#[cfg(windows)]
pub(super) fn find(
    id: &windows::core::HSTRING,
) -> windows::core::Result<windows::Media::SpeechSynthesis::VoiceInformation> {
    use windows::Media::SpeechSynthesis::SpeechSynthesizer;
    let voices = SpeechSynthesizer::AllVoices()?;
    for i in 0..voices.Size()? {
        let voice = voices.GetAt(i)?;
        if voice.Id()? == *id {
            return Ok(voice);
        }
    }
    Err(windows::core::Error::from_hresult(windows::core::HRESULT(
        E_INVALIDARG,
    )))
}

#[cfg(windows)]
fn catalog() -> Vec<Voice> {
    use windows::Media::SpeechSynthesis::{SpeechSynthesizer, VoiceGender};
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
    use windows::core::{HSTRING, Result};
    fn read() -> Result<Vec<Voice>> {
        let voices = SpeechSynthesizer::AllVoices()?;
        let c = |s: HSTRING| {
            CString::new(s.to_string_lossy()).map_err(|_| {
                windows::core::Error::from_hresult(windows::core::HRESULT(E_INVALIDARG))
            })
        };
        let mut catalog = Vec::new();
        for i in 0..voices.Size()? {
            let v = voices.GetAt(i)?;
            catalog.push(Voice {
                description: c(v.Description()?)?,
                name: c(v.DisplayName()?)?,
                id: c(v.Id()?)?,
                language: c(v.Language()?)?,
                gender: u32::from(v.Gender()? == VoiceGender::Male),
            });
        }
        Ok(catalog)
    }
    std::thread::spawn(|| -> Result<Vec<Voice>> {
        // SAFETY: this private thread owns the apartment. read releases its objects first.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        let result = read();
        unsafe { CoUninitialize() };
        result
    })
    .join()
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_and_close_are_local_to_one_synthesizer() {
        let (mut first, mut second) = (std::ptr::null(), std::ptr::null());
        assert_eq!(create(&OBJECT, &mut first), S_OK);
        assert_eq!(create(&OBJECT, &mut second), S_OK);
        assert_eq!(choose(first, Some("voice A".into())), S_OK);
        assert_eq!(selected(second), Some(None));
        assert_eq!(
            set_custom_voice(&OBJECT, first, c"missing-test-voice".as_ptr()),
            E_INVALIDARG
        );
        assert_eq!(selected(first), Some(Some("voice A".into())));
        assert_eq!(set_default_voice(&OBJECT, first), S_OK);
        assert_eq!(selected(first), Some(None));
        close(&OBJECT, first);
        assert_eq!(selected(first), None);
        assert_eq!(selected(second), Some(None));
        close(&OBJECT, second);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an installed Windows voice"]
    fn enumerate_select_and_speak() {
        extern "system" fn first(info: *const Info, context: *mut c_void) -> bool {
            // SAFETY: the enumerator supplies live strings; context is our vector.
            unsafe {
                let ids = &mut *context.cast::<Vec<CString>>();
                ids.push(std::ffi::CStr::from_ptr((*info).id).to_owned());
            }
            false
        }
        let mut ids = Vec::<CString>::new();
        assert_eq!(
            enumerate_voices(&OBJECT, (&raw mut ids).cast(), Some(first)),
            S_OK
        );
        assert_eq!(ids.len(), 1);
        let mut handle = std::ptr::null();
        assert_eq!(create(&OBJECT, &mut handle), S_OK);
        assert_eq!(set_custom_voice(&OBJECT, handle, ids[0].as_ptr()), S_OK);
        let mut stream = std::ptr::null();
        assert_eq!(
            stream_from_text(&OBJECT, handle, c"Hello world".as_ptr(), &mut stream),
            S_OK
        );
        assert!(file(stream).unwrap()[44..].iter().any(|&b| b != 0));
        close_stream(&OBJECT, stream);
        close(&OBJECT, handle);
    }
}
