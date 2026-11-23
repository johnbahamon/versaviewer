//! Remote Engine: UI-independent session logic shared by every app.
//!
//! * [`agent`] — the controlled side: registration, one-time password,
//!   explicit consent, screen streaming and input injection.
//! * [`viewer`] — the controlling side: connection, decoding and input
//!   forwarding. A UI plugs in through a callback + an input channel.
//!
//! Apps (CLI today, Flutter later via FFI) must contain no session logic.

pub mod agent;
pub mod credentials;
pub mod viewer;

pub use vv_codec::{DecodedFrame, EncoderSettings};
pub use vv_gpu::EncoderChoice;
pub use vv_protocol::control::{ControlMessage, MouseButton};
pub use vv_transport::TransportConfig;
