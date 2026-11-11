//! Bounded UI effects on Windows' built-in XAudio2, with no interruption or disk I/O.
use std::{ffi::c_void, ptr::null_mut, sync::{mpsc, OnceLock}, time::{Duration, Instant}};

type Handle = *mut c_void;
type Hr = i32;
const GROUP_VOICES: usize = 1;
const CLICK_VOICES: usize = 4;
const VOICES: usize = GROUP_VOICES + CLICK_VOICES;
// Check for stalled playback and discard stale input after this interval.
const IDLE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug)]
pub enum EventSound { Click, GroupSwitch }

impl EventSound {
    fn wav(self) -> &'static [u8] {
        match self {
            Self::Click => include_bytes!("../assets/click.wav"),
            Self::GroupSwitch => include_bytes!("../assets/group-switch.wav"),
        }
    }
}

static OUTPUT: OnceLock<Option<mpsc::SyncSender<(Option<EventSound>,Instant)>>> = OnceLock::new();

fn sender() -> Option<&'static mpsc::SyncSender<(Option<EventSound>,Instant)>> {
    OUTPUT.get_or_init(|| {
        let (tx,rx) = mpsc::sync_channel(VOICES);
        std::thread::Builder::new().name("zaplaunch-audio".into()).spawn(move || {
            let com = unsafe { super::CoInitializeEx(null_mut(),0) };
            if com < 0 { return; }
            let mut output: Option<AudioOutput> = None;
            let mut active = None;
            let mut pending_group = None;
            loop {
                if active.is_some_and(|since: Instant| since.elapsed() > Duration::from_secs(1))
                    && output.as_ref().is_some_and(|output| !output.is_idle()) {
                    output = None;
                    active = pending_group.map(|_| Instant::now());
                }
                let command = if active.is_some() {
                    let timeout = if pending_group.is_some() { Duration::from_millis(8) } else { IDLE };
                    match rx.recv_timeout(timeout) {
                        Ok(command) => command,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) if pending_group.is_some() => {
                            (Some(EventSound::GroupSwitch), pending_group.take().unwrap())
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if output.as_ref().map(|o| o.is_idle()).unwrap_or(true) { active = None; }
                            continue;
                        }
                    }
                } else { match rx.recv() { Ok(command) => command, Err(_) => break } };
                let (sound,at): (Option<EventSound>,Instant) = command;
                if matches!(sound, Some(EventSound::GroupSwitch)) { pending_group = None; }
                if at.elapsed() > IDLE {
                    if super::trace_enabled() { eprintln!("[audio] stale request skipped: {sound:?}"); }
                    continue;
                }
                if output.is_none() {
                    output = AudioOutput::new().ok();
                    if sound.is_none() {
                        if let Some(audio) = &output {
                            // Submit 20 ms of silence to wake the endpoint before the first cue.
                            static SILENCE: [u8; 882] = [0; 882];
                            if submit(audio.voices[GROUP_VOICES], &SILENCE).is_ok() {
                                active = Some(Instant::now());
                            }
                        }
                    }
                }
                let Some(sound) = sound else { continue; };
                let mut result = output.as_mut().map(|o| o.play(sound)).unwrap_or(Err(-1));
                if result.is_err() {
                    drop(output.take());
                    output = AudioOutput::new().ok();
                    result = output.as_mut().map(|o| o.play(sound)).unwrap_or(Err(-1));
                }
                if result == Ok(true) { active = Some(Instant::now()); }
                if result == Ok(false) && matches!(sound, EventSound::GroupSwitch) {
                    pending_group = Some(at);
                    continue;
                }
                if super::trace_enabled() {
                    eprintln!("[audio] {sound:?} submit={result:?} elapsed_ms={:.2}",at.elapsed().as_secs_f64()*1000.0);
                }
            }
            drop(output);
            unsafe { super::CoUninitialize(); }
        }).ok()?;
        Some(tx)
    }).as_ref()
}

/// Prepare off the UI thread before the first effect; failure can retry on the next request.
pub fn warm_event_sounds() {
    if let Some(tx) = sender() { let _ = tx.try_send((None,Instant::now())); }
}

/// Returns whether a bounded request was accepted; never blocks the UI or plays a default beep.
pub fn play_event_sound(sound: EventSound) -> bool {
    let accepted = sender().map(|tx| tx.try_send((Some(sound),Instant::now())).is_ok()).unwrap_or(false);
    if !accepted && super::trace_enabled() { eprintln!("[audio] request rejected: {sound:?}"); }
    accepted
}

#[repr(C, packed(2))]
struct Format { tag:u16, channels:u16, rate:u32, bytes:u32, align:u16, bits:u16, extra:u16 }
#[repr(C, packed(1))]
struct Buffer { flags:u32, bytes:u32, data:*const u8, begin:u32, length:u32, loop_begin:u32, loop_length:u32, loops:u32, context:Handle }
#[repr(C, packed(1))]
#[derive(Default)]
struct State { context:Handle, queued:u32, samples:u64 }

// ABI from xaudio2.h (2.9): IXAudio2 includes IUnknown; IXAudio2Voice does not.
#[repr(C)]
struct EngineApi {
    unused:[usize;2], release:unsafe extern "system" fn(Handle)->u32, callbacks:[usize;2],
    source:unsafe extern "system" fn(Handle,*mut Handle,*const Format,u32,f32,Handle,Handle,Handle)->Hr,
    submix:usize,
    master:unsafe extern "system" fn(Handle,*mut Handle,u32,u32,u32,*const u16,Handle,i32)->Hr,
    start:usize, stop:unsafe extern "system" fn(Handle),
}
#[repr(C)]
struct VoiceApi {
    unused:[usize;18], destroy:unsafe extern "system" fn(Handle),
    start:unsafe extern "system" fn(Handle,u32,u32)->Hr, stop:usize,
    submit:unsafe extern "system" fn(Handle,*const Buffer,Handle)->Hr, skipped:[usize;3],
    state:unsafe extern "system" fn(Handle,*mut State,u32),
}

#[link(name="kernel32")]
extern "system" { fn LoadLibraryExW(name:*const u16,file:Handle,flags:u32)->Handle; }

struct AudioOutput { library:Handle, engine:Handle, master:Handle, voices:[Handle;VOICES] }

unsafe fn voice_api(voice:Handle) -> &'static VoiceApi { &**(voice as *const *const VoiceApi) }
fn success(result:Hr) -> Result<(),Hr> {
    if result < 0 {
        if super::trace_enabled() { eprintln!("[audio] HRESULT=0x{:08x}",result as u32); }
        Err(result)
    } else { Ok(()) }
}

fn submit(voice: Handle, pcm: &'static [u8]) -> Result<(), Hr> {
    let buffer = Buffer { flags:0x40,bytes:pcm.len() as u32,data:pcm.as_ptr(),begin:0,length:0,loop_begin:0,loop_length:0,loops:0,context:null_mut() };
    unsafe { success((voice_api(voice).submit)(voice, &buffer, null_mut())) }
}

impl AudioOutput {
    fn new() -> Result<Self,Hr> {
        unsafe {
            let library = LoadLibraryExW(super::wide("xaudio2_9.dll").as_ptr(),null_mut(),0x800);
            if library.is_null() { return Err(-1); }
            let mut output = Self { library, engine:null_mut(), master:null_mut(), voices:[null_mut();VOICES] };
            let address = super::GetProcAddress(library,b"XAudio2Create\0".as_ptr()).ok_or(-1)?;
            let create: unsafe extern "system" fn(*mut Handle,u32,u32)->Hr = std::mem::transmute(address);
            success(create(&mut output.engine,0,1))?;
            success((output.api().master)(output.engine,&mut output.master,0,0,0,std::ptr::null(),null_mut(),5))?;
            let format = Format { tag:1,channels:1,rate:22050,bytes:44100,align:2,bits:16,extra:0 };
            for index in 0..VOICES {
                success((output.api().source)(output.engine,&mut output.voices[index],&format,0,1.0,null_mut(),null_mut(),null_mut()))?;
                let voice = output.voices[index];
                success((voice_api(voice).start)(voice,0,0))?;
            }
            // Keep the engine running across idle periods so the next short cue starts on a warm endpoint.
            Ok(output)
        }
    }

    unsafe fn api(&self) -> &EngineApi { &**(self.engine as *const *const EngineApi) }

    fn play(&mut self,sound:EventSound) -> Result<bool,Hr> {
        let pcm = pcm(sound.wav()).ok_or(-1)?;
        unsafe {
            let (group_voices, click_voices) = self.voices.split_at(GROUP_VOICES);
            let voices = match sound {
                EventSound::GroupSwitch => group_voices,
                EventSound::Click => click_voices,
            };
            for &voice in voices {
                let mut state = State::default();
                (voice_api(voice).state)(voice,&mut state,0x100);
                if state.queued != 0 { continue; }
                submit(voice, pcm)?;
                return Ok(true);
            }
        }
        // The worker keeps only the latest pending group switch. Busy click requests are dropped.
        Ok(false)
    }

    fn is_idle(&self) -> bool {
        unsafe {
            for &voice in &self.voices {
                let mut state = State::default();
                (voice_api(voice).state)(voice,&mut state,0x100);
                if state.queued != 0 { return false; }
            }
        }
        true
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        unsafe {
            if !self.engine.is_null() { (self.api().stop)(self.engine); }
            for &voice in &self.voices { if !voice.is_null() { (voice_api(voice).destroy)(voice); } }
            if !self.master.is_null() { (voice_api(self.master).destroy)(self.master); }
            if !self.engine.is_null() { (self.api().release)(self.engine); }
            super::FreeLibrary(self.library);
        }
    }
}

fn pcm(wav:&[u8]) -> Option<&[u8]> {
    if wav.get(..4)? != b"RIFF" || wav.get(8..12)? != b"WAVE"
        || u32::from_le_bytes(wav.get(4..8)?.try_into().ok()?) as usize + 8 != wav.len() { return None; }
    let mut offset = 12;
    let mut format = false;
    let mut data = None;
    while offset + 8 <= wav.len() {
        let len = u32::from_le_bytes(wav[offset+4..offset+8].try_into().ok()?) as usize;
        let body = wav.get(offset+8..offset.checked_add(8+len)?)?;
        match &wav[offset..offset+4] {
            b"fmt " => format = body.get(..16) == Some(&[1,0,1,0,34,86,0,0,68,172,0,0,2,0,16,0]),
            b"data" => data = Some(body),
            _ => {}
        }
        offset = offset.checked_add(8+len+(len & 1))?;
    }
    data.filter(|data| format && !data.is_empty() && data.len()%2 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_pcm_and_native_abi_are_valid() {
        assert_eq!(std::mem::size_of::<Format>(),18);
        assert_eq!(std::mem::size_of::<Buffer>(),44);
        assert_eq!(std::mem::size_of::<State>(),20);
        assert!(pcm(EventSound::Click.wav()).is_some());
        assert!(pcm(EventSound::GroupSwitch.wav()).is_some());
        assert_ne!(EventSound::Click.wav(),EventSound::GroupSwitch.wav());
        for len in [0,4,12,24,100] { assert!(pcm(&EventSound::Click.wav()[..len]).is_none()); }
        let mut invalid = EventSound::Click.wav().to_vec(); invalid[12..16].copy_from_slice(b"bad!");
        assert!(pcm(&invalid).is_none());
    }

    #[test]
    fn native_voices_finish_without_interrupting_each_other() {
        if std::env::var_os("ZAPLAUNCH_AUDIO_NATIVE_TEST").is_none() { return; }
        unsafe { assert!(super::super::CoInitializeEx(null_mut(),0) >= 0); }
        {
            let mut output = AudioOutput::new().expect("native XAudio2 output must open");
            for _ in 0..GROUP_VOICES { assert_eq!(output.play(EventSound::GroupSwitch),Ok(true)); }
            assert_eq!(output.play(EventSound::GroupSwitch),Ok(false),"busy group sound must not overlap");
            for _ in 0..CLICK_VOICES { assert_eq!(output.play(EventSound::Click),Ok(true)); }
            assert_eq!(output.play(EventSound::Click),Ok(false));
            assert!(!output.is_idle());
            let started = Instant::now();
            while !output.is_idle() {
                assert!(started.elapsed()<Duration::from_secs(2));
                std::thread::sleep(Duration::from_millis(10));
            }
            let group_bytes = pcm(EventSound::GroupSwitch.wav()).unwrap().len();
            let group_duration = Duration::from_secs_f64(group_bytes as f64 / 44100.0);
            assert!(started.elapsed() + Duration::from_millis(50) >= group_duration);
            std::thread::sleep(IDLE + Duration::from_millis(50));
            assert_eq!(output.play(EventSound::GroupSwitch),Ok(true));
            assert_eq!(output.play(EventSound::Click),Ok(true));
            let resumed = Instant::now();
            while !output.is_idle() {
                assert!(resumed.elapsed() < Duration::from_secs(2), "effects after idle must drain");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        unsafe { super::super::CoUninitialize(); }
    }
}
