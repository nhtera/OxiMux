// Wire-format crate shared by the app (`crates/pty`) and the relay
// daemon (`crates/relay`). Pure data + framing — no I/O, no async.

pub mod endpoint;
pub mod error;
pub mod frame;
pub mod messages;

pub use endpoint::{Endpoint, endpoint_for, namespaced_name};
pub use error::{ErrCode, ProtoError};
pub use frame::{Frame, FrameKind, MAX_FRAME_SIZE, decode_frame, encode_frame};
pub use messages::{
    Hello, HelloAck, Notification, PROTOCOL_VERSION, PtyDescriptor, PtyStats, Request, Response,
};
