//! The refusal a room operation returns: an HTTP status and an i18n key.
#[derive(Clone, Debug)]
pub struct RoomError {
    pub status: u16,
    pub key: &'static str,
}
impl RoomError {
    pub(crate) fn new(status: u16, key: &'static str) -> Self {
        Self { status, key }
    }
    /// The code a log names this refusal by (`error.type`), never its key or its text.
    pub fn code(&self) -> &'static str {
        crate::error::CoreError::Refused(self.key).code()
    }
}
