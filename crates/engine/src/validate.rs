use url::Url;

/// Validate a user-supplied download URL.
///
/// Returns the normalised URL string on success. Rejects:
/// * non-http/https schemes (file, javascript, data, ftp, ...)
/// * URLs without a host
/// * URLs carrying embedded credentials (they would otherwise leak into logs)
/// * URLs longer than 8 KiB
pub fn validate_url(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("URL is empty".to_string());
    }
    if trimmed.len() > 8192 {
        return Err("URL is too long".to_string());
    }
    let parsed = Url::parse(trimmed).map_err(|e| format!("Invalid URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => return Err(format!("Unsupported URL scheme: {other}")),
    }
    if parsed.host_str().is_none_or(|h| h.is_empty()) {
        return Err("URL has no host".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URLs with embedded credentials are not accepted".to_string());
    }
    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_http_https() {
        assert!(validate_url("https://example.com/file.zip").is_ok());
        assert!(validate_url("http://example.com/file.zip").is_ok());
    }
    #[test]
    fn edge_cases_empty_long_and_non_english() {
        assert!(validate_url("").is_err());
        assert!(validate_url("   ").is_err());
        assert!(validate_url(&format!("https://example.com/{}", "a".repeat(9000))).is_err());
        // Non-English path is fine; it is percent-encoded by the URL parser.
        assert!(validate_url("https://example.com/تقرير.pdf").is_ok());
        assert!(validate_url("https://example.com/café 🎉.pdf").is_ok());
    }
    #[test]
    fn rejects_dangerous_schemes() {
        assert!(validate_url("file:///C:/Windows/win.ini").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("ftp://example.com/x").is_err());
        assert!(validate_url("https://user:pass@example.com/x").is_err());
        assert!(validate_url("https://").is_err());
    }
}
