//! Minimal bindings to eSpeak NG in synchronous mode.
//!
//! eSpeak NG is a global, single-threaded library: [`Espeak`] must be created and used on
//! one thread only, which the speech engine guarantees by owning it in its own thread.

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_short, c_uint, c_void};
use std::path::Path;

#[allow(non_camel_case_types)]
mod ffi {
    use super::*;

    pub type espeak_ng_STATUS = c_uint;
    pub const ENS_OK: espeak_ng_STATUS = 0;
    pub const ENS_SPEECH_STOPPED: espeak_ng_STATUS = 0x10000EFF;
    pub const ENOUTPUT_MODE_SYNCHRONOUS: c_int = 0x0001;
    pub const POS_CHARACTER: c_int = 1;
    pub const ESPEAK_CHARS_UTF8: c_uint = 1;
    pub const ESPEAK_SSML: c_uint = 0x10;

    pub const EVENT_LIST_TERMINATED: c_int = 0;
    pub const EVENT_MARK: c_int = 3;

    pub const PARAM_RATE: c_int = 1;
    pub const PARAM_VOLUME: c_int = 2;
    pub const PARAM_PITCH: c_int = 3;
    pub const PARAM_RANGE: c_int = 4;
    pub const PARAM_PUNCTUATION: c_int = 5;
    pub const PARAM_CAPITALS: c_int = 6;

    #[repr(C)]
    pub union EventId {
        pub number: c_int,
        pub name: *const c_char,
        pub string: [c_char; 8],
    }

    #[repr(C)]
    pub struct espeak_EVENT {
        pub kind: c_int,
        pub unique_identifier: c_uint,
        pub text_position: c_int,
        pub length: c_int,
        pub audio_position: c_int,
        pub sample: c_int,
        pub user_data: *mut c_void,
        pub id: EventId,
    }

    pub type SynthCallback =
        unsafe extern "C" fn(wav: *mut c_short, samples: c_int, events: *mut espeak_EVENT) -> c_int;

    unsafe extern "C" {
        pub fn espeak_ng_InitializePath(path: *const c_char);
        pub fn espeak_ng_Initialize(context: *mut c_void) -> espeak_ng_STATUS;
        pub fn espeak_ng_InitializeOutput(
            mode: c_int,
            buffer_length: c_int,
            device: *const c_char,
        ) -> espeak_ng_STATUS;
        pub fn espeak_ng_GetSampleRate() -> c_int;
        pub fn espeak_ng_SetParameter(
            parameter: c_int,
            value: c_int,
            relative: c_int,
        ) -> espeak_ng_STATUS;
        pub fn espeak_ng_SetVoiceByName(name: *const c_char) -> espeak_ng_STATUS;
        pub fn espeak_ng_Synthesize(
            text: *const c_void,
            size: usize,
            position: c_uint,
            position_type: c_int,
            end_position: c_uint,
            flags: c_uint,
            unique_identifier: *mut c_uint,
            user_data: *mut c_void,
        ) -> espeak_ng_STATUS;
        pub fn espeak_ng_SpeakCharacter(character: u32) -> espeak_ng_STATUS;
        pub fn espeak_ng_SpeakKeyName(name: *const c_char) -> espeak_ng_STATUS;
        pub fn espeak_ng_Terminate() -> espeak_ng_STATUS;
        pub fn espeak_SetSynthCallback(callback: SynthCallback);
    }
}

/// Receives the output of a synthesis call.
pub trait SynthSink {
    /// A chunk of 16-bit mono samples. Returning `false` aborts the synthesis.
    fn samples(&mut self, samples: &[i16]) -> bool;
    /// A `<mark name="..."/>` was reached, `position_ms` after the start of the utterance.
    fn mark(&mut self, name: &str, position_ms: u32);
}

thread_local! {
    /// The sink of the synthesis running on this thread. eSpeak NG's callback carries no
    /// context of its own.
    static SINK: RefCell<Option<*mut dyn SynthSink>> = const { RefCell::new(None) };
}

unsafe extern "C" fn synth_callback(
    wav: *mut c_short,
    samples: c_int,
    events: *mut ffi::espeak_EVENT,
) -> c_int {
    SINK.with(|sink| {
        let Some(sink) = *sink.borrow() else {
            return 1;
        };
        // SAFETY: the pointer is set by `Espeak::run` for the duration of the synthesis
        // call that invokes this callback, on this thread.
        let sink = unsafe { &mut *sink };

        let mut event = events;
        while !event.is_null() {
            // SAFETY: eSpeak NG passes an array terminated by EVENT_LIST_TERMINATED.
            let current = unsafe { &*event };
            if current.kind == ffi::EVENT_LIST_TERMINATED {
                break;
            }
            if current.kind == ffi::EVENT_MARK {
                // SAFETY: for mark events `id.name` is a NUL terminated UTF-8 string.
                let name = unsafe { CStr::from_ptr(current.id.name) };
                sink.mark(
                    &name.to_string_lossy(),
                    current.audio_position.max(0) as u32,
                );
            }
            event = unsafe { event.add(1) };
        }

        if !wav.is_null() && samples > 0 {
            // SAFETY: eSpeak NG passes `samples` valid samples.
            let chunk = unsafe { std::slice::from_raw_parts(wav, samples as usize) };
            if !sink.samples(chunk) {
                return 1;
            }
        }
        0
    })
}

#[derive(Debug, thiserror::Error)]
#[error("eSpeak NG error {0:#x} in {1}")]
pub struct EspeakError(c_uint, &'static str);

fn check(status: ffi::espeak_ng_STATUS, what: &'static str) -> Result<(), EspeakError> {
    if status == ffi::ENS_OK {
        Ok(())
    } else {
        Err(EspeakError(status, what))
    }
}

pub struct Espeak {
    sample_rate: u32,
    // eSpeak NG is not thread safe.
    _not_send: std::marker::PhantomData<*const ()>,
}

/// Speech parameters in eSpeak NG's units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parameters {
    /// Words per minute, 80..=450.
    pub rate: i32,
    /// 0..=200, 100 is normal.
    pub volume: i32,
    /// 0..=100, 50 is normal.
    pub pitch: i32,
    /// 0..=100, 50 is normal.
    pub range: i32,
    /// 0 none, 1 all, 2 some.
    pub punctuation: i32,
    /// 0 none, 1 sound, 2 spell, 3+ raise pitch.
    pub capitals: i32,
}

impl Espeak {
    /// Initializes eSpeak NG with the `espeak-ng-data` directory inside `data_parent`.
    /// `chunk_ms` is the length of the chunks passed to the sink.
    pub fn new(data_parent: &Path, chunk_ms: u32) -> Result<Espeak, EspeakError> {
        let path = CString::new(data_parent.as_os_str().as_encoded_bytes())
            .map_err(|_| EspeakError(0, "data path"))?;
        unsafe {
            ffi::espeak_ng_InitializePath(path.as_ptr());
            check(
                ffi::espeak_ng_Initialize(std::ptr::null_mut()),
                "initialization",
            )?;
            check(
                ffi::espeak_ng_InitializeOutput(
                    ffi::ENOUTPUT_MODE_SYNCHRONOUS,
                    chunk_ms as c_int,
                    std::ptr::null(),
                ),
                "output initialization",
            )?;
            ffi::espeak_SetSynthCallback(synth_callback);
        }
        let espeak = Espeak {
            sample_rate: unsafe { ffi::espeak_ng_GetSampleRate() } as u32,
            _not_send: std::marker::PhantomData,
        };
        espeak.set_voice("en")?;
        Ok(espeak)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Selects a voice by name or language code, e.g. `en-us` or `sk`.
    pub fn set_voice(&self, name: &str) -> Result<(), EspeakError> {
        let name = CString::new(name).map_err(|_| EspeakError(0, "voice name"))?;
        check(
            unsafe { ffi::espeak_ng_SetVoiceByName(name.as_ptr()) },
            "voice selection",
        )
    }

    pub fn set_parameters(&self, parameters: &Parameters) -> Result<(), EspeakError> {
        let set = |parameter, value, what| {
            check(
                unsafe { ffi::espeak_ng_SetParameter(parameter, value, 0) },
                what,
            )
        };
        set(ffi::PARAM_RATE, parameters.rate, "rate")?;
        set(ffi::PARAM_VOLUME, parameters.volume, "volume")?;
        set(ffi::PARAM_PITCH, parameters.pitch, "pitch")?;
        set(ffi::PARAM_RANGE, parameters.range, "pitch range")?;
        // eSpeak NG 1.52 stores these two and then reports EINVAL, because its setter has
        // no further work to do for them; the value does take effect.
        unsafe {
            ffi::espeak_ng_SetParameter(ffi::PARAM_PUNCTUATION, parameters.punctuation, 0);
            ffi::espeak_ng_SetParameter(ffi::PARAM_CAPITALS, parameters.capitals, 0);
        }
        Ok(())
    }

    /// Synthesizes SSML, blocking until done or aborted by the sink. Returns whether the
    /// synthesis completed.
    pub fn speak_ssml(&self, ssml: &str, sink: &mut dyn SynthSink) -> Result<bool, EspeakError> {
        let text = CString::new(ssml).map_err(|_| EspeakError(0, "text"))?;
        let bytes = text.as_bytes_with_nul();
        self.run(sink, || unsafe {
            ffi::espeak_ng_Synthesize(
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
                ffi::POS_CHARACTER,
                0,
                ffi::ESPEAK_CHARS_UTF8 | ffi::ESPEAK_SSML,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        })
    }

    /// Speaks a single character the way eSpeak NG names characters.
    pub fn speak_character(&self, c: char, sink: &mut dyn SynthSink) -> Result<bool, EspeakError> {
        self.run(sink, || unsafe { ffi::espeak_ng_SpeakCharacter(c as u32) })
    }

    /// Speaks a key name such as `space` or `shift_a`.
    pub fn speak_key(&self, name: &str, sink: &mut dyn SynthSink) -> Result<bool, EspeakError> {
        let name = CString::new(name).map_err(|_| EspeakError(0, "key name"))?;
        self.run(sink, || unsafe {
            ffi::espeak_ng_SpeakKeyName(name.as_ptr())
        })
    }

    fn run(
        &self,
        sink: &mut dyn SynthSink,
        synthesize: impl FnOnce() -> ffi::espeak_ng_STATUS,
    ) -> Result<bool, EspeakError> {
        // SAFETY: the sink outlives the synthesis call; the pointer is cleared before
        // returning, and the lifetime is only erased to fit in the thread local.
        let sink: *mut (dyn SynthSink + '_) = sink;
        let sink: *mut (dyn SynthSink + 'static) = unsafe { std::mem::transmute(sink) };
        SINK.with(|slot| *slot.borrow_mut() = Some(sink));
        let status = synthesize();
        SINK.with(|slot| *slot.borrow_mut() = None);
        match status {
            ffi::ENS_SPEECH_STOPPED => Ok(false),
            status => check(status, "synthesis").map(|()| true),
        }
    }
}

impl Drop for Espeak {
    fn drop(&mut self) {
        unsafe { ffi::espeak_ng_Terminate() };
    }
}

/// Voice data built for the development machine, when this is a native build.
pub fn dev_data() -> Option<&'static Path> {
    option_env!("RCONNECT_ESPEAK_DEV_DATA").map(Path::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Collect {
        samples: usize,
        marks: Vec<(String, u32)>,
        abort_after: Option<usize>,
    }

    impl SynthSink for Collect {
        fn samples(&mut self, samples: &[i16]) -> bool {
            self.samples += samples.len();
            self.abort_after.is_none_or(|limit| self.samples < limit)
        }

        fn mark(&mut self, name: &str, position_ms: u32) {
            self.marks.push((name.to_string(), position_ms));
        }
    }

    /// eSpeak NG is global, so everything runs in one test on one thread.
    #[test]
    fn synthesizes_with_marks_and_aborts() {
        let Some(data) = dev_data() else { return };
        let espeak = Espeak::new(data, 20).unwrap();
        assert_eq!(espeak.sample_rate(), 22050);

        let mut sink = Collect::default();
        let completed = espeak
            .speak_ssml(
                "<speak><mark name=\"0\"/>Hello <mark name=\"1\"/>world</speak>",
                &mut sink,
            )
            .unwrap();
        assert!(completed);
        assert!(sink.samples > 22050 / 4, "only {} samples", sink.samples);
        assert_eq!(sink.marks.len(), 2);
        assert_eq!(sink.marks[0].0, "0");
        assert!(sink.marks[1].1 > sink.marks[0].1);

        let mut aborted = Collect {
            abort_after: Some(2000),
            ..Default::default()
        };
        let completed = espeak
            .speak_ssml(
                "<speak>A much longer sentence that gets cut off early.</speak>",
                &mut aborted,
            )
            .unwrap();
        assert!(!completed);
        assert!(aborted.samples < 22050 / 2);

        let mut character = Collect::default();
        espeak.speak_character('a', &mut character).unwrap();
        assert!(character.samples > 0);

        espeak
            .set_parameters(&Parameters {
                rate: 300,
                volume: 100,
                pitch: 50,
                range: 50,
                punctuation: 2,
                capitals: 1,
            })
            .unwrap();
        espeak.set_voice("sk").unwrap();
        let mut slovak = Collect::default();
        espeak
            .speak_ssml("<speak>Dobrý deň</speak>", &mut slovak)
            .unwrap();
        assert!(slovak.samples > 0);
    }
}
