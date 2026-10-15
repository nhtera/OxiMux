//! Watching a device on the desktop over the FFI: the RPCs, and the pump that
//! reads each view stream and hands its frames to the app's [`SimViewSink`].
//!
//! The core does not decode. A frame crosses as it arrived (an avcC record or
//! an AVCC sample), and the app feeds it to the platform's decoder. Nothing is
//! kept: a frame is passed on and dropped, never written anywhere.

use std::sync::{Arc, Weak};

use oximux_remote_proto::proto::SIMULATOR_VIEW_MIN_VERSION;
use oximux_remote_proto::simulator::{SimErrorWire, SimViewClassWire, read_view_frame, read_view_token};
use futures::future::{Either, select};
use oximux_remote_proto::UniRecv;
use oximux_remote_proto::proto::RpcError;
use oximux_remote_session::{SessionClosed, SessionError, ViewStreams};

use crate::callbacks::SimViewSink;
use crate::client::{MobileClient, Shared};
use crate::ffi_types::MobileError;
use crate::runtime::rt;

/// A device the phone may watch.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SimViewable {
    pub device: String,
    pub name: String,
    pub worktree: String,
    /// `ios-simulator`, `android-emulator`, `iphone` or `android-phone`.
    pub class: String,
    /// A real device: its screen can show notifications and codes.
    pub physical: bool,
    /// Streaming on the desktop now; one that is not cannot be watched.
    pub streaming: bool,
}

/// The list, or why the desktop shows none (viewing switched off there).
#[derive(Debug, Clone, uniffi::Record)]
pub struct SimViewList {
    pub devices: Vec<SimViewable>,
    pub refused: Option<String>,
}

/// How a subscribe went.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum SimSubscribeOutcome {
    /// Frames for `token` are on their way to the sink.
    Started { token: String, width: u32, height: u32 },
    /// The desktop asked its user; ask again shortly.
    Pending,
    /// The desktop's user said no; asking again is pointless for a while.
    Denied { retry_after_secs: u64 },
    /// Not possible now, and why (in words for the screen).
    Refused { message: String },
}

fn class_name(class: SimViewClassWire) -> &'static str {
    match class {
        SimViewClassWire::IosSimulator => "ios-simulator",
        SimViewClassWire::AndroidEmulator => "android-emulator",
        SimViewClassWire::Iphone => "iphone",
        SimViewClassWire::AndroidPhone => "android-phone",
    }
}

/// The desktop's refusal, as the phone shows it.
fn refusal(e: &SimErrorWire) -> String {
    match e {
        SimErrorWire::NotStreaming => "The device is not streaming on the desktop. Show it there, then try again.".into(),
        SimErrorWire::NoDevice | SimErrorWire::NotFound(_) => "That device is no longer attached on the desktop.".into(),
        SimErrorWire::Refused(why) | SimErrorWire::Failed(why) | SimErrorWire::Unavailable(why) => why.clone(),
        other => other.to_string(),
    }
}

/// The phone may not watch at all: revoked, narrowed to some sessions, or
/// forgotten on the desktop. Said as a refusal, so the app does not keep
/// asking.
const NOT_ALLOWED: &str = "This phone may no longer watch the desktop's devices. Pair it again with full access to watch.";

fn hex(token: &[u8; 16]) -> String {
    token.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(token: &str) -> Option<[u8; 16]> {
    if token.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(token.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Accept the view streams of one connection until it or its session ends,
/// each read on its own task. Spawned per (re)connect, beside the terminal
/// pump.
///
/// Holds neither the session nor the client's state: the state holds the
/// session, and the session is what has to be dropped for this to end (and
/// for its connection to close). Holding either would be a cycle that keeps
/// a dead connection open.
pub(crate) async fn run_view_pump(shared: Weak<Shared>, views: ViewStreams) {
    while let Some(stream) = views.next().await {
        rt().spawn(read_view(shared.clone(), stream, views.closed()));
    }
}

/// Read one view stream into the sink, until it ends or its session is
/// dropped. Dropping the stream (no sink, the session gone) tells the host to
/// stop sending.
async fn read_view(shared: Weak<Shared>, mut stream: Box<dyn UniRecv>, closed: SessionClosed) {
    let token = match select(Box::pin(read_view_token(stream.as_mut())), closed.clone()).await {
        Either::Left((Ok(Some(token)), _)) => token,
        _ => return,
    };
    let token = hex(&token);
    loop {
        let frame = match select(Box::pin(read_view_frame(stream.as_mut())), closed.clone()).await {
            Either::Left((frame, _)) => frame,
            Either::Right(_) => Ok(None),
        };
        // Read per frame, like the terminal sink: the app may register it
        // after frames started, so a frame with no sink yet is skipped, not
        // the end of the view.
        let Some(shared) = shared.upgrade() else { return };
        let sink = shared.sim_view_sink.lock().unwrap().clone();
        drop(shared);
        match frame {
            Ok(Some(frame)) => {
                if let Some(sink) = sink {
                    sink.on_frame(token.clone(), frame.kind as u8, frame.pts_us, frame.payload);
                }
            }
            Ok(None) | Err(_) => {
                if let Some(sink) = sink {
                    sink.on_end(token);
                }
                return;
            }
        }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl MobileClient {
    /// Register the sink that receives every view stream's frames.
    pub fn set_sim_view_sink(&self, sink: Arc<dyn SimViewSink>) {
        *self.shared.sim_view_sink.lock().unwrap() = Some(sink);
    }

    /// Whether the host can show a device at all: the Devices screen is
    /// offered only then.
    pub fn host_supports_sim_view(&self) -> bool {
        self.shared.session().is_ok_and(|s| s.host_version() >= SIMULATOR_VIEW_MIN_VERSION)
    }

    /// Whether the connection runs through a relay (the view's badge).
    pub fn connection_relayed(&self) -> Option<bool> {
        self.shared.session().ok().and_then(|s| s.relayed())
    }

    /// The devices this phone may watch.
    pub async fn list_sim_viewables(&self) -> Result<SimViewList, MobileError> {
        let session = self.shared.session()?;
        let answer = session.sim_viewables().await.map_err(|e| MobileError::Rpc(e.to_string()))?;
        Ok(match answer {
            Ok(rows) => SimViewList {
                devices: rows
                    .into_iter()
                    .map(|r| SimViewable {
                        physical: r.class.is_physical(),
                        class: class_name(r.class).into(),
                        device: r.device,
                        name: r.name,
                        worktree: r.worktree,
                        streaming: r.streaming,
                    })
                    .collect(),
                refused: None,
            },
            Err(e) => SimViewList { devices: Vec::new(), refused: Some(refusal(&e)) },
        })
    }

    /// Start watching `device`.
    pub async fn subscribe_sim_view(&self, device: String) -> Result<SimSubscribeOutcome, MobileError> {
        let session = self.shared.session()?;
        let answer = match session.sim_subscribe(&device).await {
            Ok(answer) => answer,
            Err(SessionError::Rpc(RpcError::Unauthorized)) => {
                return Ok(SimSubscribeOutcome::Refused { message: NOT_ALLOWED.into() });
            }
            Err(e) => return Err(MobileError::Rpc(e.to_string())),
        };
        Ok(match answer {
            Ok(sub) => SimSubscribeOutcome::Started { token: hex(&sub.token), width: sub.width, height: sub.height },
            Err(SimErrorWire::ConsentPending) => SimSubscribeOutcome::Pending,
            Err(SimErrorWire::ConsentDenied { retry_after_secs }) => SimSubscribeOutcome::Denied { retry_after_secs },
            Err(e) => SimSubscribeOutcome::Refused { message: refusal(&e) },
        })
    }

    /// Stop watching. Idempotent; a token that is not one is ignored.
    pub async fn unsubscribe_sim_view(&self, token: String) -> Result<(), MobileError> {
        let Some(token) = unhex(&token) else { return Ok(()) };
        let session = self.shared.session()?;
        session.sim_unsubscribe(token).await.map_err(|e| MobileError::Rpc(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_cross_as_hex_and_back() {
        let token = [0, 1, 0xab, 0xff, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 0x10, 0x7f];
        let text = hex(&token);
        assert_eq!(text.len(), 32);
        assert_eq!(unhex(&text), Some(token));
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex(&"g".repeat(32)), None);
    }

    #[test]
    fn the_desktops_refusals_read_as_the_phone_shows_them() {
        assert!(refusal(&SimErrorWire::NotStreaming).contains("not streaming on the desktop"));
        assert_eq!(refusal(&SimErrorWire::Refused("Viewing is off.".into())), "Viewing is off.");
        assert!(refusal(&SimErrorWire::NotFound("x".into())).contains("no longer attached"));
    }
}
