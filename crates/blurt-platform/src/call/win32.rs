//! Windows universal mute, through WinRT's `VoipCallCoordinator`.
//!
//! While blurt is in a call, it holds a VoIP call with the system, which
//! shows the microphone button in the taskbar for it and sends Win+Alt+K
//! its way. Requests come in on `MuteStateChanged`, which the system raises
//! on a thread of its own; blurt answers with `NotifyMuted` or
//! `NotifyUnmuted`.
//!
//! The coordinator lives on a thread of its own, in the multithreaded
//! apartment, which the async side hands new states. Closing the channel
//! to it ends the call and lets go of the coordinator.

use std::sync::mpsc as std_mpsc;

use futures_channel::{mpsc, oneshot};
use futures_util::StreamExt;
use windows::ApplicationModel::Calls::{
    MuteChangeEventArgs, VoipCallCoordinator, VoipPhoneCall, VoipPhoneCallMedia,
};
use windows::Foundation::TypedEventHandler;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::HSTRING;

use super::{Event, State, Step, steps};

pub async fn run(
    events: &mpsc::UnboundedSender<Event>,
    mut inbox: mpsc::UnboundedReceiver<State>,
) -> Result<(), String> {
    let (to_thread, from_app) = std_mpsc::channel();
    let (ready, started) = oneshot::channel();
    std::thread::Builder::new()
        .name("blurt-call".into())
        .spawn({
            let events = events.clone();
            move || thread(events, from_app, ready)
        })
        .map_err(|e| e.to_string())?;
    started
        .await
        .map_err(|_| "the call thread stopped".to_owned())??;
    log::info!("Universal mute ready");
    while let Some(mut state) = inbox.next().await {
        // Only the latest matters.
        while let Ok(newer) = inbox.try_recv() {
            state = newer;
        }
        if to_thread.send(state).is_err() {
            return Err("the call thread stopped".into());
        }
    }
    // Dropping `to_thread` ends any call, here or if this is dropped.
    Ok(())
}

fn thread(
    events: mpsc::UnboundedSender<Event>,
    inbox: std_mpsc::Receiver<State>,
    ready: oneshot::Sender<Result<(), String>>,
) {
    if let Err(e) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        let _ = ready.send(Err(format!("WinRT didn't start: {}", e.message())));
        return;
    }
    match Calls::new(events) {
        Ok(mut calls) => {
            let _ = ready.send(Ok(()));
            for next in inbox {
                calls.apply(next);
            }
            calls.apply(State::default());
        }
        Err(e) => {
            let _ = ready.send(Err(format!("no VoIP calls here: {}", e.message())));
        }
    }
    unsafe { RoUninitialize() };
}

/// The coordinator, and the call blurt holds with it.
struct Calls {
    coordinator: VoipCallCoordinator,
    /// For `RemoveMuteStateChanged`.
    token: i64,
    /// What the system was last told.
    now: State,
    /// `None` while there's no call, or if the system refused it.
    call: Option<VoipPhoneCall>,
}

impl Calls {
    fn new(events: mpsc::UnboundedSender<Event>) -> windows::core::Result<Self> {
        let coordinator = VoipCallCoordinator::GetDefault()?;
        let handler =
            TypedEventHandler::<VoipCallCoordinator, MuteChangeEventArgs>::new(move |_, args| {
                let muted = args.ok()?.Muted()?;
                log::debug!("The system asks to {}mute", if muted { "" } else { "un" });
                let _ = events.unbounded_send(Event::MuteRequested(muted));
                Ok(())
            });
        let token = coordinator.MuteStateChanged(&handler)?;
        Ok(Self {
            coordinator,
            token,
            now: State::default(),
            call: None,
        })
    }

    fn apply(&mut self, next: State) {
        for step in steps(&self.now, &next) {
            match step {
                Step::Start(name) => {
                    self.call = self
                        .start(&name)
                        .inspect_err(|e| log::warn!("The system refused the call: {e}"))
                        .ok();
                }
                Step::End => {
                    if let Some(call) = self.call.take()
                        && let Err(e) = call.NotifyCallEnded()
                    {
                        log::debug!("Could not end the call: {e}");
                    }
                }
                Step::Notify { muted } => {
                    let told = if muted {
                        self.coordinator.NotifyMuted()
                    } else {
                        self.coordinator.NotifyUnmuted()
                    };
                    if let Err(e) = told {
                        log::debug!("Could not tell the system about mute: {e}");
                    }
                }
            }
        }
        self.now = next;
    }

    fn start(&self, name: &str) -> windows::core::Result<VoipPhoneCall> {
        let call = self.coordinator.RequestNewOutgoingCall(
            &HSTRING::from("blurt"),
            &HSTRING::from(name),
            &HSTRING::from("blurt"),
            VoipPhoneCallMedia::Audio,
        )?;
        call.NotifyCallActive()?;
        Ok(call)
    }
}

impl Drop for Calls {
    fn drop(&mut self) {
        if let Err(e) = self.coordinator.RemoveMuteStateChanged(self.token) {
            log::debug!("Could not stop hearing mute requests: {e}");
        }
    }
}
