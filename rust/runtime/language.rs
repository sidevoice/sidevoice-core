/// The process language from the locale environment, for rendering process-level messages.
pub fn system_language() -> String {
    std::env::var("LC_ALL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("LANG").ok())
        .unwrap_or_else(|| "en".to_owned())
}
