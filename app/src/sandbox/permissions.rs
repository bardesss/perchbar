//! Network allow-list enforcement. Every widget request, and every redirect
//! hop, must pass `check_url` against the hosts the manifest declared.

use url::Url;

/// Original request plus at most 5 redirect hops.
const MAX_CHAIN: usize = 6;

/// Parses `url` and checks it against the declared hostnames. The scheme must
/// be `https`, the port the default one, there must be no userinfo, and the
/// host must equal a declared entry (case-insensitive, no subdomain matching).
pub fn check_url(url: &str, allowed: &[String]) -> Result<Url, String> {
    let parsed = Url::parse(url).map_err(|e| format!("Perchbar: invalid URL: {e}"))?;
    let host = parsed.host_str().unwrap_or("").to_string();
    if parsed.scheme() != "https" {
        return Err(format!("Perchbar: only https is allowed: {host}"));
    }
    if parsed.port().is_some() || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!("Perchbar: host not allowed: {host}"));
    }
    if host.is_empty() || !allowed.iter().any(|a| a.eq_ignore_ascii_case(&host)) {
        return Err(format!("Perchbar: host not allowed: {host}"));
    }
    Ok(parsed)
}

/// True if the whole chain (original URL first) is permitted and short enough.
pub fn redirect_allowed(chain: &[Url], allowed: &[String]) -> bool {
    chain.len() <= MAX_CHAIN && chain.iter().all(|u| check_url(u.as_str(), allowed).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow() -> Vec<String> {
        vec!["api.coinbase.com".to_string()]
    }

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn allows_exact_host() {
        let url = check_url("https://api.coinbase.com/v2/x", &allow()).unwrap();
        assert_eq!(url.host_str(), Some("api.coinbase.com"));
    }

    #[test]
    fn rejects_other_host() {
        let err = check_url("https://evil.com/", &allow()).unwrap_err();
        assert_eq!(err, "Perchbar: host not allowed: evil.com");
    }

    #[test]
    fn rejects_userinfo_trick() {
        let err = check_url("https://api.coinbase.com@evil.com/", &allow()).unwrap_err();
        assert!(err.contains("host not allowed: evil.com"), "{err}");
    }

    #[test]
    fn rejects_subdomain_not_declared() {
        assert!(check_url("https://x.api.coinbase.com/", &allow()).is_err());
    }

    #[test]
    fn host_match_is_case_insensitive() {
        assert!(check_url("https://API.Coinbase.com/", &allow()).is_ok());
        let mixed = vec!["API.Coinbase.com".to_string()];
        assert!(check_url("https://api.coinbase.com/", &mixed).is_ok());
    }

    #[test]
    fn rejects_non_https() {
        assert!(check_url("http://api.coinbase.com/", &allow()).is_err());
    }

    #[test]
    fn rejects_non_default_port() {
        assert!(check_url("https://api.coinbase.com:8443/", &allow()).is_err());
        assert!(check_url("https://api.coinbase.com:443/", &allow()).is_ok());
    }

    #[test]
    fn rejects_ip_literal_unless_declared() {
        assert!(check_url("https://127.0.0.1/", &allow()).is_err());
        assert!(check_url("https://127.0.0.1/", &["127.0.0.1".to_string()]).is_ok());
    }

    #[test]
    fn rejects_unparseable_url() {
        assert!(check_url("not a url", &allow()).is_err());
    }

    #[test]
    fn redirect_to_undeclared_host_blocked() {
        let ok = [
            u("https://api.coinbase.com/a"),
            u("https://api.coinbase.com/b"),
        ];
        assert!(redirect_allowed(&ok, &allow()));
        let bad = [u("https://api.coinbase.com/a"), u("https://evil.com/b")];
        assert!(!redirect_allowed(&bad, &allow()));
    }

    #[test]
    fn more_than_five_redirects_blocked() {
        let hop = |n: usize| -> Vec<Url> {
            (0..n)
                .map(|i| u(&format!("https://api.coinbase.com/{i}")))
                .collect()
        };
        assert!(redirect_allowed(&hop(6), &allow()));
        assert!(!redirect_allowed(&hop(7), &allow()));
    }
}
