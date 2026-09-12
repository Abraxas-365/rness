use super::*;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    url: String,
}
impl FetchConfig {
    pub(super) async fn fetch(&self, args: Value) -> Result<Value, String> {
        let args: Args = serde_json::from_value(args).map_err(|e| e.to_string())?;
        let mut current = url(&args.url)?;
        for hop in 0..=self.max_redirects {
            let host = current
                .host_str()
                .ok_or("URL missing host")?
                .trim_matches(['[', ']']);
            let port = current.port_or_known_default().ok_or("URL missing port")?;
            let addresses = public_addresses(host, port).await?;
            let client = client()
                .resolve_to_addrs(host, &addresses)
                .build()
                .map_err(|e| e.to_string())?;
            let response = client
                .get(current.clone())
                .header(
                    "accept",
                    "text/html,application/xhtml+xml,text/*;q=0.9,application/json;q=0.8",
                )
                .send()
                .await
                .map_err(|_| "HTTP fetch request failed".to_string())?;
            let status = response.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                if hop == self.max_redirects {
                    return Err("HTTP fetch exceeded redirect limit".into());
                }
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|h| h.to_str().ok())
                    .ok_or("redirect missing Location")?;
                current = redirect(&current, location)?;
                continue;
            }
            if !status.is_success() {
                return Err(format!("HTTP fetch returned {status}"));
            }
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .unwrap_or("")
                .to_owned();
            let (html, encoding) = classify(&content_type)?;
            let bytes = body(response, self.max_response_bytes).await?;
            let (decoded, _, _) = encoding.decode(&bytes);
            let truncated = decoded.chars().count() > self.max_body_chars;
            let decoded: String = decoded.chars().take(self.max_body_chars).collect();
            let content = if html { markdown(&decoded)? } else { decoded };
            let truncated = truncated || content.chars().count() > self.max_body_chars;
            let content: String = content.chars().take(self.max_body_chars).collect();
            return Ok(
                json!({"url":current.as_str(),"contentType":content_type,"content":content,"truncated":truncated}),
            );
        }
        unreachable!()
    }
}
fn markdown(html: &str) -> Result<String, String> {
    // Bound parser work conservatively before invoking the HTML tree builder.
    if html.bytes().filter(|b| *b == b'<').count() > 10_000 {
        return Err("HTML exceeds conversion complexity limit".into());
    }
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec![
            "script", "style", "noscript", "template", "iframe", "object", "embed",
        ])
        .build();
    let tree = converter
        .html_to_tree(html)
        .map_err(|_| "HTML parsing failed")?;
    let mut pending = vec![(tree.clone(), 0)];
    while let Some((node, depth)) = pending.pop() {
        if depth > 512 {
            return Err("HTML exceeds conversion depth limit".into());
        }
        if let markup5ever_rcdom::NodeData::Element { name, attrs, .. } = &node.data {
            let hidden = attrs.borrow().iter().any(|a| {
                let key = a.name.local.as_ref();
                let value = a.value.to_ascii_lowercase();
                key == "hidden"
                    || (key == "aria-hidden" && value == "true")
                    || (name.local.as_ref() == "input" && key == "type" && value == "hidden")
                    || (key == "style"
                        && value.split(';').any(|declaration| {
                            declaration.split_once(':').is_some_and(|(key, value)| {
                                let value = value.trim().trim_end_matches("!important").trim();
                                (key.trim() == "display" && value == "none")
                                    || (key.trim() == "visibility"
                                        && matches!(value, "hidden" | "collapse"))
                            })
                        }))
            });
            if hidden {
                node.children.borrow_mut().clear();
                continue;
            }
        }
        pending.extend(
            node.children
                .borrow()
                .iter()
                .map(|child| (child.clone(), depth + 1)),
        );
    }
    Ok(converter.tree_to_markdown(&tree))
}
fn redirect(current: &Url, location: &str) -> Result<Url, String> {
    let target = current
        .join(location)
        .map_err(|_| "invalid redirect URL".to_string())?;
    let target = url(target.as_str())?;
    if current.origin() != target.origin() {
        return Err(format!(
            "cross-origin redirect blocked; make a separate web_fetch call for {target}"
        ));
    }
    Ok(target)
}
fn classify(content_type: &str) -> Result<(bool, &'static encoding_rs::Encoding), String> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let html = matches!(mime.as_str(), "text/html" | "application/xhtml+xml");
    if !html
        && !mime.starts_with("text/")
        && !matches!(mime.as_str(), "application/json" | "application/xml")
        && !mime.ends_with("+json")
        && !mime.ends_with("+xml")
    {
        return Err("unsupported HTTP content type (binary downloads are not supported)".into());
    }
    let charset = content_type.split(';').skip(1).find_map(|p| {
        p.split_once('=')
            .filter(|(k, _)| k.trim().eq_ignore_ascii_case("charset"))
            .map(|(_, v)| v.trim().trim_matches('"'))
    });
    let encoding = match charset {
        Some(label) => {
            encoding_rs::Encoding::for_label(label.as_bytes()).ok_or("unsupported charset")?
        }
        None => encoding_rs::UTF_8,
    };
    Ok((html, encoding))
}
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 168 || b == 0 || (b == 88 && c == 99)))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(v4.into());
            }
            let s = ip.segments();
            // Only global unicast, excluding transition, protocol-assignment and documentation ranges.
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x0200 || s[1] == 0x0db8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}
fn embedded(ip: Ipv6Addr, length: usize) -> Option<Ipv4Addr> {
    let bytes = ip.octets();
    if length == 96 {
        return Some(Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]));
    }
    if bytes[8] != 0 {
        return None;
    }
    let start = length / 8;
    let before = 8 - start;
    let mut v4 = [0; 4];
    v4[..before].copy_from_slice(&bytes[start..8]);
    v4[before..].copy_from_slice(&bytes[9..9 + 4 - before]);
    Some(v4.into())
}
async fn public_addresses(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let addresses: Vec<_> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| "DNS lookup failed")?
            .collect(),
    };
    if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
        return Err("URL resolves to a non-public or empty address set".into());
    }
    // Discover network-specific DNS64 prefixes before allowing IPv6 destinations.
    if addresses.iter().any(SocketAddr::is_ipv6) {
        let discovery: Vec<_> = tokio::net::lookup_host(("ipv4only.arpa", 80))
            .await
            .map_err(|_| "DNS64 safety discovery failed")?
            .collect();
        for sentinel in discovery.iter().filter_map(|a| {
            if let IpAddr::V6(ip) = a.ip() {
                Some(ip)
            } else {
                None
            }
        }) {
            for length in [32, 40, 48, 56, 64, 96] {
                if !embedded(sentinel, length)
                    .is_some_and(|ip| matches!(ip.octets(), [192, 0, 0, 170] | [192, 0, 0, 171]))
                {
                    continue;
                }
                for destination in &addresses {
                    if let IpAddr::V6(ip) = destination.ip() {
                        if ip.octets()[..length / 8] == sentinel.octets()[..length / 8]
                            && embedded(ip, length).is_some_and(|v4| !public_ip(v4.into()))
                        {
                            return Err(
                                "NAT64 destination resolves to a non-public IPv4 address".into()
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_network_policy() {
        for blocked in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
            "2002:a00:1::",
            "2001:db8::1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(!public_ip(blocked.parse().unwrap()), "{blocked}");
        }
        for allowed in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(public_ip(allowed.parse().unwrap()));
        }
        for blocked in [
            "file:///etc/passwd",
            "http://user:password@example.com",
            "http://127.1",
        ] {
            if let Ok(url) = url(blocked) {
                assert!(!public_ip(url.host_str().unwrap().parse().unwrap()));
            }
        }
        let origin = url("https://example.com/a").unwrap();
        assert!(redirect(&origin, "/b").is_ok());
        assert!(redirect(&origin, "http://example.com/b").is_err());
        assert!(redirect(&origin, "https://other.com").is_err());
        assert!(redirect(&origin, "https://user@example.com").is_err());
    }
    #[test]
    fn markdown_removes_nonvisible_content() {
        let output = markdown("<h1>Title</h1><script>secret</script><div hidden>hidden</div><p style='display: none !important'>invisible</p><p>Visible <a href='https://example.com'>link</a></p>").unwrap();
        assert!(output.contains("# Title"));
        assert!(output.contains("https://example.com"));
        for hidden in ["secret", "hidden", "invisible"] {
            assert!(!output.contains(hidden));
        }
        assert!(markdown(&"<div>".repeat(10001)).is_err());
    }
    #[test]
    fn content_types_and_charset() {
        assert!(classify("image/png").is_err());
        assert!(classify("").is_err());
        assert!(classify("text/plain; charset=unknown").is_err());
        let (html, enc) = classify("text/html; charset=windows-1252").unwrap();
        assert!(html);
        assert_eq!(enc.decode(&[0xe9]).0, "é");
        assert!(!classify("application/problem+json").unwrap().0);
    }
    #[tokio::test]
    async fn rejects_private_before_connecting() {
        assert!(FetchConfig::default()
            .fetch(json!({"url":"http://127.0.0.1:1"}))
            .await
            .unwrap_err()
            .contains("non-public"));
        assert!(FetchConfig::default()
            .fetch(json!({"url":"http://[::1]:1"}))
            .await
            .unwrap_err()
            .contains("non-public"));
    }
}
