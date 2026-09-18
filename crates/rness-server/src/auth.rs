//! Single-user bearer authentication. This is not a multi-tenant boundary.
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

#[derive(Clone)]
pub struct Auth(Option<String>);
impl Auth {
    pub fn for_bind(address: std::net::SocketAddr, token: Option<String>) -> Result<Self, String> {
        if token
            .as_ref()
            .is_some_and(|token| token.len() < 32 || !token.bytes().all(|b| b.is_ascii_graphic()))
        {
            return Err(
                "RNESS_SERVER_TOKEN must contain at least 32 printable non-space ASCII characters"
                    .into(),
            );
        }
        if !address.ip().is_loopback() && token.is_none() {
            return Err("non-loopback serving requires RNESS_SERVER_TOKEN; use a TLS reverse proxy for remote access".into());
        }
        Ok(Self(token))
    }
}

pub async fn authorize(State(auth): State<Auth>, request: Request, next: Next) -> Response {
    // This API has no browser client. Deny browser-originated cross-site calls,
    // including requests to unauthenticated loopback deployments.
    if request.headers().contains_key("origin")
        || request
            .headers()
            .get("sec-fetch-site")
            .is_some_and(|value| value != "none")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if auth.0.is_none() {
        let local_authority = |authority: &axum::http::uri::Authority| {
            let host = authority.host();
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        };
        let hosts: Vec<_> = request.headers().get_all("host").iter().collect();
        let valid_host = hosts.len() == 1
            && hosts[0]
                .to_str()
                .ok()
                .and_then(|host| host.parse::<axum::http::uri::Authority>().ok())
                .is_some_and(|host| local_authority(&host));
        if !valid_host
            || request
                .uri()
                .authority()
                .is_some_and(|authority| !local_authority(authority))
        {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    if let Some(expected) = &auth.0 {
        let supplied = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let valid = supplied.is_some_and(|supplied| {
            // Compare all token bytes without a data-dependent early exit.
            let mut difference = supplied.len() ^ expected.len();
            for (i, byte) in expected.bytes().enumerate() {
                difference |= usize::from(byte ^ supplied.as_bytes().get(i).copied().unwrap_or(0));
            }
            difference == 0
        });
        if !valid {
            return (StatusCode::UNAUTHORIZED, [("www-authenticate", "Bearer")]).into_response();
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_binds_require_strong_explicit_tokens() {
        assert!(Auth::for_bind("127.0.0.1:0".parse().unwrap(), None).is_ok());
        assert!(Auth::for_bind("[::1]:0".parse().unwrap(), None).is_ok());
        assert!(Auth::for_bind("0.0.0.0:0".parse().unwrap(), None).is_err());
        assert!(Auth::for_bind("[::]:0".parse().unwrap(), Some("short".into())).is_err());
        assert!(Auth::for_bind("0.0.0.0:0".parse().unwrap(), Some("a".repeat(32))).is_ok());
    }
}
