//! Fixtures shared by the ElevenLabs tests: recorded payloads, mock servers and the local client.
use crate::providers::elevenlabs::{CloudSpeech, ElevenLabsTts};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

const MODELS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/rust_t4/eleven_models.json"
));
pub(super) const SPARSE_MODELS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/rust_t4/eleven_models_sparse.json"
));
pub(super) const VOICES_ONE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/rust_t4/eleven_voices_page_one.json"
));
pub(super) const VOICES_TWO: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/rust_t4/eleven_voices_page_two.json"
));
pub(super) const SPARSE_VOICES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/rust_t4/eleven_voices_sparse.json"
));
pub(super) const LEGACY_VOICES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/rust_t4/eleven_voices_legacy.json"
));
pub(super) const TIMESTAMPS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/rust_t4/eleven_timestamps.json"
));
pub(super) const TIMESTAMP_CHUNKS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/rust_t4/eleven_timestamp_chunks.json"
));

pub(super) fn local_tts(server: &MockServer, timeout: Duration) -> ElevenLabsTts {
    ElevenLabsTts::with_config("test-key", &server.uri(), timeout).unwrap()
}

pub(super) async fn split_timestamp_server() -> (String, JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{address}");
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0, "SDK closed before sending request headers");
            request.extend_from_slice(&buffer[..read]);
        }

        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;

        let body = TIMESTAMP_CHUNKS.as_bytes();
        let first_record_end = body.iter().position(|byte| *byte == b'\n').unwrap() + 1;
        let first_fragment_end = 17;
        write_http_chunk(&mut socket, &body[..first_fragment_end]).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        write_http_chunk(&mut socket, &body[first_fragment_end..first_record_end]).await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        write_http_chunk(&mut socket, &body[first_record_end..]).await;
        socket.write_all(b"0\r\n\r\n").await.unwrap();
        request
    });
    (base_url, task)
}

pub(super) async fn delayed_body_server(timestamped: bool) -> (String, JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{address}");
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).await.unwrap();
            assert_ne!(read, 0, "SDK closed before sending request headers");
            request.extend_from_slice(&buffer[..read]);
        }

        if timestamped {
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            write_http_chunk(&mut socket, b"{\"audio_base64\":").await;
        } else {
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 32\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            socket.write_all(b"mp3").await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        request
    });
    (base_url, task)
}

async fn write_http_chunk(socket: &mut tokio::net::TcpStream, bytes: &[u8]) {
    socket
        .write_all(format!("{:X}\r\n", bytes.len()).as_bytes())
        .await
        .unwrap();
    socket.write_all(bytes).await.unwrap();
    socket.write_all(b"\r\n").await.unwrap();
}

pub(super) async fn mount_models(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("xi-api-key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(MODELS))
        .mount(server)
        .await;
}

pub(super) fn assert_audio(speech: &CloudSpeech, expected: &[u8]) {
    assert_eq!(speech.audio, expected);
    assert_eq!(speech.mime_type, "audio/mpeg");
}
