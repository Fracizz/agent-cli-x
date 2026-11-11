//! 统一错误类型：同时用于服务端 HTTP 响应和客户端进程退出码。

use std::fmt;

#[derive(Debug, Clone)]
pub struct AppError {
    pub code: String,
    pub message: String,
}

pub type Result<T> = std::result::Result<T, AppError>;

impl AppError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new("bad_request", m)
    }
    pub fn denied(m: impl Into<String>) -> Self {
        Self::new("policy_denied", m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new("not_found", m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new("conflict", m)
    }
    pub fn auth(m: impl Into<String>) -> Self {
        Self::new("auth", m)
    }
    pub fn connect(m: impl Into<String>) -> Self {
        Self::new("connect", m)
    }
    pub fn fingerprint(m: impl Into<String>) -> Self {
        Self::new("fingerprint", m)
    }
    pub fn io(m: impl Into<String>) -> Self {
        Self::new("io", m)
    }
    pub fn config(m: impl Into<String>) -> Self {
        Self::new("config", m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new("internal", m)
    }

    /// 客户端进程退出码。远端命令的退出码原样透传，agent-cli-x 自身错误使用 100 以上的保留码。
    pub fn exit_code(&self) -> i32 {
        match self.code.as_str() {
            "connect" => 100,
            "auth" => 101,
            "policy_denied" => 102,
            "not_found" => 103,
            "timeout" => 104,
            "conflict" => 105,
            "fingerprint" => 106,
            "io" => 107,
            "config" => 109,
            "bad_request" => 110,
            "busy" => 111,
            _ => 108,
        }
    }

    pub fn http_status(&self) -> u16 {
        match self.code.as_str() {
            "bad_request" => 400,
            "auth" => 401,
            "policy_denied" => 403,
            "not_found" => 404,
            "timeout" => 408,
            "conflict" => 409,
            "busy" => 503,
            _ => 500,
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error[{}]: {}", self.code, self.message)
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => AppError::not_found(e.to_string()),
            _ => AppError::io(e.to_string()),
        }
    }
}
