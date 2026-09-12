use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Args {
    query: String,
    #[serde(default)]
    max_results: Option<usize>,
}
impl SearchConfig {
    fn credentials(&self) -> Option<(&Option<String>, &str, &str)> {
        match self {
            Self::Exa {
                api_key,
                api_key_env,
                base_url,
                ..
            }
            | Self::Perplexity {
                api_key,
                api_key_env,
                base_url,
                ..
            }
            | Self::Deepseek {
                api_key,
                api_key_env,
                base_url,
                ..
            } => Some((api_key, api_key_env, base_url)),
            Self::Duckduckgo {} => None,
        }
    }
    pub(super) fn validate(&self) -> Result<(), String> {
        let Some((_, env, base)) = self.credentials() else {
            return Ok(());
        };
        let endpoint = url(base)?;
        if endpoint.scheme() != "https" {
            return Err("search endpoints must use HTTPS".into());
        }
        if endpoint.query().is_some() || endpoint.fragment().is_some() {
            return Err("search base_url cannot have a query or fragment".into());
        }
        if env.is_empty() || env.contains(['=', '\0']) {
            return Err("invalid web search api_key_env".into());
        }
        match self {
            Self::Exa {
                search_type,
                highlights_per_result,
                ..
            } if !matches!(search_type.as_str(), "auto" | "keyword" | "neural")
                || *highlights_per_result == 0 =>
            {
                Err("invalid Exa search_type or highlights_per_result".into())
            }
            Self::Perplexity {
                model,
                max_tokens,
                search_recency,
                ..
            } if model.trim().is_empty()
                || *max_tokens == 0
                || search_recency
                    .as_ref()
                    .is_some_and(|s| !matches!(s.as_str(), "day" | "week" | "month" | "year")) =>
            {
                Err("invalid Perplexity model, max_tokens or search_recency".into())
            }
            Self::Deepseek {
                model,
                max_tokens,
                max_uses,
                api_version,
                ..
            } if model.trim().is_empty()
                || *max_tokens == 0
                || *max_uses == 0
                || api_version.trim().is_empty() =>
            {
                Err("invalid DeepSeek model or request limits".into())
            }
            _ => Ok(()),
        }
    }
    fn request(&self, query: &str, max_results: Option<usize>) -> (&'static str, Value) {
        match self {
            Self::Duckduckgo {} => unreachable!("DuckDuckGo uses anonymous GET requests"),
            Self::Exa {
                search_type,
                highlights_per_result,
                ..
            } => {
                let mut body = json!({"query":query,"type":search_type,"contents":{"highlights":{"highlightsPerUrl":highlights_per_result}}});
                if let Some(n) = max_results {
                    body["numResults"] = json!(n);
                }
                ("search", body)
            }
            Self::Perplexity {
                model,
                max_tokens,
                search_recency,
                ..
            } => {
                let mut body = json!({"model":model,"max_tokens":max_tokens,"messages":[{"role":"user","content":query}]});
                if let Some(recency) = search_recency {
                    body["search_recency_filter"] = json!(recency);
                }
                ("chat/completions", body)
            }
            Self::Deepseek {
                model,
                max_tokens,
                max_uses,
                ..
            } => (
                "messages",
                json!({"model":model,"max_tokens":max_tokens,"messages":[{"role":"user","content":[{"type":"text","text":format!("Perform a web search for the query: {query}")}]}],"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":max_uses}]}),
            ),
        }
    }
    pub(super) async fn search(&self, args: Value) -> Result<SearchResult, String> {
        let args: Args = serde_json::from_value(args).map_err(|e| e.to_string())?;
        if args.query.trim().is_empty() || args.max_results.is_some_and(|n| n == 0 || n > 100) {
            return Err("query must be nonblank and maxResults must be 1..100".into());
        }
        let Some((literal, env, base)) = self.credentials() else {
            let response = client()
                .timeout(Duration::from_secs(15))
                .build()
                .map_err(|e| e.to_string())?
                .get("https://html.duckduckgo.com/html/")
                .query(&[("q", &args.query)])
                .send()
                .await
                .map_err(|_| "DuckDuckGo search request failed")?;
            if response.status().as_u16() != 200 {
                return Err(format!(
                    "DuckDuckGo search returned HTTP {}; search may be rate-limited or challenged",
                    response.status()
                ));
            }
            let bytes = body(response, 512 * 1024).await?;
            return parse_duckduckgo(
                &String::from_utf8_lossy(&bytes),
                args.max_results.unwrap_or(10),
            );
        };
        let key = literal
            .clone()
            .or_else(|| std::env::var(env).ok())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                format!("web search requires {env}; no fallback backend is configured")
            })?;
        let (path, payload) = self.request(&args.query, args.max_results);
        let mut request = client()
            .build()
            .map_err(|e| e.to_string())?
            .post(format!("{}/{path}", base.trim_end_matches('/')))
            .bearer_auth(&key)
            .header("accept", "application/json")
            .json(&payload);
        if let Self::Deepseek { api_version, .. } = self {
            request = request
                .header("x-api-key", &key)
                .header("anthropic-version", api_version);
        }
        let response = request
            .send()
            .await
            .map_err(|_| "web search request failed".to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "web search returned HTTP {}; redirects are not followed",
                response.status()
            ));
        }
        let value = serde_json::from_slice(&body(response, 5_000_000).await?)
            .map_err(|_| "invalid search response JSON".to_string())?;
        let mut result = self.map(value)?;
        if let Some(limit) = args.max_results {
            result.truncated = result.sources.len() > limit;
            result.sources.truncate(limit);
        }
        Ok(result)
    }
    fn map(&self, value: Value) -> Result<SearchResult, String> {
        let mut result = SearchResult {
            sources: vec![],
            content: None,
            truncated: false,
        };
        match self {
            Self::Duckduckgo {} => return Err("DuckDuckGo returns HTML, not JSON".into()),
            Self::Exa { .. } => {
                for item in array(&value, "results")? {
                    let Some(snippet) =
                        item.get("highlights")
                            .and_then(Value::as_array)
                            .and_then(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .find(|s| !s.trim().is_empty())
                            })
                    else {
                        continue;
                    };
                    result
                        .sources
                        .push(source(item, Some(snippet.to_owned()), "publishedDate")?);
                }
            }
            Self::Perplexity { .. } => {
                result.content = value
                    .pointer("/choices/0/message/content")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                if value.get("search_results").is_some() {
                    for item in array(&value, "search_results")? {
                        result
                            .sources
                            .push(source(item, text(item, "snippet"), "date")?);
                    }
                } else if let Some(citations) = value.get("citations") {
                    for citation in citations.as_array().ok_or("invalid citations")? {
                        result.sources.push(Source {
                            url: citation.as_str().ok_or("invalid citation URL")?.into(),
                            title: None,
                            snippet: None,
                            published_at: None,
                        });
                    }
                }
                if result.content.is_none()
                    && value.get("search_results").is_none()
                    && value.get("citations").is_none()
                {
                    return Err("missing Perplexity search response".into());
                }
            }
            Self::Deepseek { .. } => {
                let blocks = array(&value, "content")?;
                let mut snippets = std::collections::HashMap::new();
                for block in blocks.iter().filter(|b| b["type"] == "text") {
                    if let Some(cites) = block["citations"].as_array() {
                        for cite in cites {
                            if let (Some(url), Some(snippet)) =
                                (text(cite, "url"), text(cite, "cited_text"))
                            {
                                snippets.entry(url).or_insert(snippet);
                            }
                        }
                    }
                }
                let mut found = false;
                let mut seen = std::collections::HashSet::new();
                for block in blocks
                    .iter()
                    .filter(|b| b["type"] == "web_search_tool_result")
                {
                    found = true;
                    if block["content"].is_object() {
                        return Err("DeepSeek native web search returned a tool error".into());
                    }
                    for item in array(block, "content")? {
                        if item["type"] != "web_search_result" {
                            continue;
                        }
                        let url = text(item, "url").ok_or("DeepSeek result missing URL")?;
                        if seen.insert(url.clone()) {
                            result.sources.push(source(
                                item,
                                snippets.get(&url).cloned(),
                                "page_age",
                            )?);
                        }
                    }
                }
                if !found {
                    return Err("DeepSeek returned no web_search_tool_result blocks; native search did not run".into());
                }
            }
        }
        Ok(result)
    }
}
fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    value[key]
        .as_array()
        .ok_or_else(|| format!("invalid search response: expected {key} array"))
}
fn text(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
fn source(value: &Value, snippet: Option<String>, published: &str) -> Result<Source, String> {
    Ok(Source {
        url: text(value, "url").ok_or("search result missing URL")?,
        title: text(value, "title"),
        snippet,
        published_at: text(value, published),
    })
}

fn parse_duckduckgo(html: &str, limit: usize) -> Result<SearchResult, String> {
    use markup5ever_rcdom::NodeData;
    let tree = htmd::HtmlToMarkdown::new()
        .html_to_tree(html)
        .map_err(|_| "invalid DuckDuckGo HTML")?;
    let mut pending = vec![tree.clone()];
    let mut sources = Vec::new();
    let mut no_results = false;
    let mut challenged = false;
    while let Some(node) = pending.pop() {
        if let NodeData::Element { attrs, .. } = &node.data {
            let attrs = attrs.borrow();
            let attr = |key: &str| {
                attrs
                    .iter()
                    .find(|a| a.name.local.as_ref() == key)
                    .map(|a| a.value.to_string())
                    .unwrap_or_default()
            };
            let class = attr("class");
            no_results |= class.split_whitespace().any(|c| c == "no-results");
            challenged |= attr("id").contains("challenge") || class.contains("anomaly-modal");
            if class.split_whitespace().any(|c| c == "result__a") {
                let href = attr("href");
                let base = Url::parse("https://html.duckduckgo.com").unwrap();
                if let Ok(mut target) = base.join(&href) {
                    if matches!(
                        target.host_str(),
                        Some("duckduckgo.com" | "html.duckduckgo.com")
                    ) && target.path() == "/l/"
                    {
                        let destination = target
                            .query_pairs()
                            .find(|(key, _)| key == "uddg")
                            .map(|(_, v)| v.into_owned());
                        let Some(destination) = destination else {
                            continue;
                        };
                        let Ok(parsed) = Url::parse(&destination) else {
                            continue;
                        };
                        target = parsed;
                    }
                    if matches!(target.scheme(), "http" | "https")
                        && target.username().is_empty()
                        && target.password().is_none()
                    {
                        sources.push(Source {
                            url: target.into(),
                            title: Some(node_text(&node)),
                            snippet: None,
                            published_at: None,
                        });
                    }
                }
            } else if class.split_whitespace().any(|c| c == "result__snippet") {
                if let Some(last) = sources.last_mut() {
                    last.snippet = Some(node_text(&node));
                }
            }
        }
        pending.extend(node.children.borrow().iter().rev().cloned());
    }
    if challenged {
        return Err(
            "DuckDuckGo bot challenge; retry later or explicitly select another backend".into(),
        );
    }
    if sources.is_empty() && !no_results {
        return Err("DuckDuckGo returned unrecognized HTML, not search results".into());
    }
    let truncated = sources.len() > limit;
    sources.truncate(limit);
    Ok(SearchResult {
        sources,
        content: None,
        truncated,
    })
}
fn node_text(node: &std::rc::Rc<htmd::Node>) -> String {
    let mut pending = vec![node.clone()];
    let mut text = String::new();
    while let Some(node) = pending.pop() {
        if let markup5ever_rcdom::NodeData::Text { contents } = &node.data {
            text.push_str(&contents.borrow());
        }
        pending.extend(node.children.borrow().iter().rev().cloned());
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duckduckgo_keyless_results_and_failures() {
        let config: Config =
            serde_json::from_value(json!({"search":{"provider":"duckduckgo"}})).unwrap();
        let registry = ToolRegistry::default();
        register(&registry, config).unwrap();
        assert!(registry.get("web_search").is_some());
        assert!(registry.get("web_fetch").is_none());
        let html = r#"<div><a class='result__a' href='//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fx%3D1%26y%3D2'>Rust &amp; <b>tools</b></a><a class='result__snippet'>A <b>useful</b> snippet.</a></div><a class='result__a' href='https://second.example'>Second</a>"#;
        let result = parse_duckduckgo(html, 1).unwrap();
        assert!(result.truncated);
        assert_eq!(result.sources[0].url, "https://example.com/a?x=1&y=2");
        assert_eq!(result.sources[0].title.as_deref(), Some("Rust & tools"));
        assert_eq!(
            result.sources[0].snippet.as_deref(),
            Some("A useful snippet.")
        );
        assert!(
            parse_duckduckgo("<div class='no-results'>No results</div>", 10)
                .unwrap()
                .sources
                .is_empty()
        );
        assert!(parse_duckduckgo("<form id='challenge-form'></form>", 10)
            .unwrap_err()
            .contains("challenge"));
        assert!(parse_duckduckgo("<html>unexpected</html>", 10).is_err());
    }

    fn config(provider: &str) -> SearchConfig {
        serde_json::from_value(json!({"provider":provider})).unwrap()
    }
    #[test]
    fn provider_contracts_and_normalization() {
        let exa = config("exa");
        assert_eq!(
            exa.request("rust", Some(3)).1,
            json!({"query":"rust","type":"auto","contents":{"highlights":{"highlightsPerUrl":1}},"numResults":3})
        );
        let result = exa.map(json!({"results":[{"url":"https://a","highlights":[" ","excerpt"]},{"url":"https://b"}]})).unwrap();
        assert_eq!(result.sources.len(), 1);
        assert_eq!(result.sources[0].snippet.as_deref(), Some("excerpt"));
        let perplexity = config("perplexity");
        assert_eq!(perplexity.request("rust", None).0, "chat/completions");
        let result = perplexity
            .map(json!({"choices":[{"message":{"content":"answer"}}],"citations":["https://a"]}))
            .unwrap();
        assert_eq!(result.content.as_deref(), Some("answer"));
        assert_eq!(result.sources.len(), 1);
        assert!(perplexity
            .map(json!({"search_results":[],"citations":["https://a"]}))
            .unwrap()
            .sources
            .is_empty());
        let deepseek = config("deepseek");
        assert_eq!(
            deepseek.request("rust", None).1["tools"][0]["type"],
            "web_search_20250305"
        );
        assert!(deepseek
            .map(json!({"content":[{"type":"text","text":"not searched"}]}))
            .is_err());
        let result = deepseek.map(json!({"content":[{"type":"text","citations":[{"url":"https://a","cited_text":"quote"}]},{"type":"web_search_tool_result","content":[{"type":"web_search_result","url":"https://a"},{"type":"web_search_result","url":"https://a"}]}]})).unwrap();
        assert_eq!(result.sources.len(), 1);
        assert_eq!(result.sources[0].snippet.as_deref(), Some("quote"));
    }
    #[tokio::test]
    async fn credentials_errors_and_cancellation() {
        let c: SearchConfig = serde_json::from_value(
            json!({"provider":"exa","api_key_env":"RNESS_TEST_WEB_ABSENT_CREDENTIAL"}),
        )
        .unwrap();
        assert!(c
            .search(json!({"query":"rust"}))
            .await
            .unwrap_err()
            .contains("RNESS_TEST_WEB_ABSENT_CREDENTIAL"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(WebTool::Search(c)
            .run(json!({"query":"rust"}), &cancel)
            .await
            .unwrap_err()
            .contains("cancelled"));
    }
}
