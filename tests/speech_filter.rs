//! The complete-turn speech gate with the real Silero model: silence, noise and clicks never reach
//! a provider; speech does. Needs the staged Rustvani models (`RUSTVANI_CACHE_DIR`, `cargo xtask models`).

use sidevoice_core::pipeline::has_speech;

fn pcm(samples: impl IntoIterator<Item = f64>) -> Vec<u8> {
    samples
        .into_iter()
        .flat_map(|sample| (sample.clamp(-32768.0, 32767.0) as i16).to_le_bytes())
        .collect()
}

/// Gaussian noise from a fixed seed (xorshift and Box-Muller), standard deviation `sigma`.
fn noise(count: usize, sigma: f64) -> Vec<u8> {
    let mut state = 42_u64;
    let mut uniform = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1_u64 << 53) as f64
    };
    pcm((0..count).map(|_| {
        let (a, b) = (uniform().max(f64::MIN_POSITIVE), uniform());
        sigma * (-2.0 * a.ln()).sqrt() * (std::f64::consts::TAU * b).cos()
    }))
}

#[tokio::test]
async fn silence_noise_and_clicks_are_not_speech() {
    let mut click = vec![0.0; 32_000];
    click[1000..1010].fill(25_000.0);
    for (name, audio) in [
        ("empty", Vec::new()),
        ("silence", pcm(vec![0.0; 32_000])),
        ("noise", noise(32_000, 300.0)),
        ("click", pcm(click)),
    ] {
        assert!(!has_speech(&audio).await.unwrap(), "{name}");
    }
}

#[tokio::test]
async fn recorded_speech_is_speech() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hola-sala-16k.wav"
    );
    let mut reader = hound::WavReader::open(path).unwrap();
    assert_eq!(
        (reader.spec().sample_rate, reader.spec().channels),
        (16_000, 1)
    );
    let audio: Vec<u8> = reader
        .samples::<i16>()
        .flat_map(|sample| sample.unwrap().to_le_bytes())
        .collect();
    assert!(has_speech(&audio).await.unwrap());
    assert!(
        has_speech(&audio).await.unwrap(),
        "the same words may be repeated"
    );
}
