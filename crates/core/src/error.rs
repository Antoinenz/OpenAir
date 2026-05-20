use thiserror::Error;

#[derive(Debug, Error)]
pub enum OpenAirError {
    #[error("discovery error: {0}")]
    Discovery(String),

    #[error("pairing error: {0}")]
    Pairing(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("rtsp error: {0}")]
    Rtsp(String),

    #[error("audio error: {0}")]
    Audio(String),

    #[error("timing error: {0}")]
    Timing(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, OpenAirError>;
