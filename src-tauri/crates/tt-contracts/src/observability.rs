use serde::Serialize;

pub const COMMAND: &str = "tauritavern::command";
pub const USER_VISIBLE_ERROR: &str = "tauritavern::user_error";
pub const BACKEND_ERROR_EVENT: &str = "tauritavern-backend-error";

#[derive(Clone, Serialize)]
pub struct BackendErrorPayload {
    pub message: String,
}
