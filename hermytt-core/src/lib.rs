pub mod buffer;
pub mod exec;
pub mod platform;
pub mod recording;
pub mod session;

pub use buffer::BufferedOutput;
pub use exec::{ExecResult, exec};
pub use recording::{RecordingHandle, RecordingInfo, start_recording, list_recordings};
pub use session::{Session, SessionHandle, SessionManager};
