//! Windows capture without drivers (MASTER.md §3.8 M4b): which apps are playing audio, and
//! WASAPI process loopback of one app (its whole process tree). Whole-system loopback needs
//! nothing here: cpal records a playback device in loopback mode.

use anyhow::{anyhow, Context};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use windows::core::{implement, IUnknown, Interface, Ref, PWSTR};
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS};
use windows::Win32::System::Threading::{CreateEventW, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::System::Variant::VT_BLOB;

/// (pid, exe file name) of every process with an audio session on an active playback device.
pub fn sessions() -> Vec<(u32, String)> {
    unsafe { list_sessions() }.unwrap_or_default()
}

unsafe fn list_sessions() -> windows::core::Result<Vec<(u32, String)>> {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED); // already initialised is fine
    let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let devs = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    let mut out = vec![];
    for i in 0..devs.GetCount()? {
        let Ok(mgr) = devs.Item(i)?.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
        let list = mgr.GetSessionEnumerator()?;
        for j in 0..list.GetCount()? {
            let Ok(s) = list.GetSession(j)?.cast::<IAudioSessionControl2>() else { continue };
            let pid = s.GetProcessId().unwrap_or(0); // 0 = system sounds
            if let Some(exe) = (pid != 0).then(|| exe_name(pid)).flatten() {
                out.push((pid, exe));
            }
        }
    }
    Ok(out)
}

unsafe fn exe_name(pid: u32) -> Option<String> {
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let (mut buf, mut len) = ([0u16; 1024], 1024u32);
    let r = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
    let _ = CloseHandle(h);
    r.ok()?;
    String::from_utf16_lossy(&buf[..len as usize]).rsplit('\\').next().map(str::to_string)
}

/// The process to capture: one that owns an audio session, else the first one running that exe.
fn pid_of(exe: &str) -> Option<u32> {
    let session = sessions().into_iter().find(|(_, n)| n.eq_ignore_ascii_case(exe)).map(|(p, _)| p);
    session.or_else(|| unsafe { first_process(exe) })
}

unsafe fn first_process(exe: &str) -> Option<u32> {
    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
    let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut found = None;
    let mut more = Process32FirstW(snap, &mut e).is_ok();
    while more && found.is_none() {
        let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
        if String::from_utf16_lossy(&e.szExeFile[..n]).eq_ignore_ascii_case(exe) {
            found = Some(e.th32ProcessID);
        }
        more = Process32NextW(snap, &mut e).is_ok();
    }
    let _ = CloseHandle(snap);
    found
}

/// A running app capture; dropping it stops the capture thread.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Captures `exe` (and its child processes) as 48 kHz float stereo, handing each packet to `sink`
/// on a dedicated thread. Fails if the app isn't running or Windows refuses the capture.
pub fn start(exe: &str, mut sink: impl FnMut(&[f32]) + Send + 'static) -> anyhow::Result<Capture> {
    let pid = pid_of(exe).ok_or_else(|| anyhow!("{} isn't running", crate::app_title(exe)))?;
    let stop = Arc::new(AtomicBool::new(false));
    let (ready, started) = channel();
    let st = stop.clone();
    let thread = std::thread::Builder::new().name("capralink-appcap".into()).spawn(move || {
        if let Err(e) = unsafe { run(pid, &st, &mut sink, &ready) } {
            eprintln!("app capture: {e:#}");
            let _ = ready.send(Err(e));
        }
    })?;
    let cap = Capture { stop, thread: Some(thread) };
    started.recv().map_err(|_| anyhow!("app capture thread died"))??;
    Ok(cap)
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(Sender<windows::core::Result<IUnknown>>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, op: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let result = op.ok().and_then(|op| unsafe {
            let (mut hr, mut client) = (Default::default(), None);
            op.GetActivateResult(&mut hr, &mut client)?;
            windows::core::HRESULT::ok(hr)?;
            client.ok_or_else(|| windows::core::Error::from(AUDCLNT_E_DEVICE_INVALIDATED))
        });
        let _ = self.0.send(result);
        Ok(())
    }
}

unsafe fn run(pid: u32, stop: &AtomicBool, sink: &mut dyn FnMut(&[f32]), ready: &Sender<anyhow::Result<()>>) -> anyhow::Result<()> {
    CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE },
        },
    };
    let mut pv = PROPVARIANT::default();
    let v = &mut *pv.Anonymous.Anonymous;
    v.vt = VT_BLOB;
    v.Anonymous.blob = BLOB { cbSize: size_of_val(&params) as u32, pBlobData: &mut params as *mut _ as *mut u8 };

    let (tx, rx) = channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(tx).into();
    let _op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&pv), &handler).context("process loopback")?;
    let client: IAudioClient = rx.recv_timeout(Duration::from_secs(5)).context("process loopback: no answer")?.context("process loopback")?.cast()?;

    // Process loopback has no mix format: we name ours and Windows converts to it.
    let fmt = WAVEFORMATEX { wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16, nChannels: 2, nSamplesPerSec: 48_000, nAvgBytesPerSec: 48_000 * 8, nBlockAlign: 8, wBitsPerSample: 32, cbSize: 0 };
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM;
    client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, &fmt, None)?; // 20 ms buffer
    let event = CreateEventW(None, false, false, None)?;
    client.SetEventHandle(event)?;
    let cap: IAudioCaptureClient = client.GetService()?;
    client.Start()?;
    let _ = ready.send(Ok(()));

    let mut silence = Vec::new();
    while !stop.load(Relaxed) {
        if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
            continue;
        }
        while cap.GetNextPacketSize()? > 0 {
            let (mut data, mut frames, mut bits) = (std::ptr::null_mut(), 0u32, 0u32);
            cap.GetBuffer(&mut data, &mut frames, &mut bits, None, None)?;
            let n = frames as usize * 2;
            if bits & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                silence.resize(n, 0.0);
                sink(&silence[..n]);
            } else {
                sink(std::slice::from_raw_parts(data as *const f32, n));
            }
            cap.ReleaseBuffer(frames)?;
        }
    }
    let _ = client.Stop();
    let _ = CloseHandle(event);
    Ok(())
}
