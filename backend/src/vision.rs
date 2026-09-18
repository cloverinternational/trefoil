//! Process-local cache for generated vision descriptors.

use std::collections::{HashMap, VecDeque};

use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_ENTRIES: usize = 500;
const MAX_IMAGE_URL_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum VisionDescriptorError {
    #[error("image URL is empty or too large")]
    InvalidImageUrl,
    #[error("descriptor request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("descriptor model returned no text")]
    EmptyResponse,
}

/// Return image URLs in an OpenAI-compatible request without assuming that
/// every `content` value is an array. Malformed parts are ignored rather than
/// causing an inference request to panic.
pub fn image_urls(body: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    fn visit(value: &Value, urls: &mut Vec<String>) {
        match value {
            Value::Array(values) => values.iter().for_each(|value| visit(value, urls)),
            Value::Object(object) => {
                if object.get("type").and_then(Value::as_str) == Some("image_url") {
                    if let Some(url) = object
                        .get("image_url")
                        .and_then(|image| image.get("url"))
                        .and_then(Value::as_str)
                    {
                        if valid_image_url(url) {
                            urls.push(url.to_owned());
                        }
                    }
                }
                if object.get("type").and_then(Value::as_str) == Some("image") {
                    if let Some(source) = object.get("source") {
                        if let Some(data) = source.get("data").and_then(Value::as_str) {
                            let media = source
                                .get("media_type")
                                .and_then(Value::as_str)
                                .unwrap_or("image/png");
                            let url = format!("data:{media};base64,{data}");
                            if valid_image_url(&url) {
                                urls.push(url);
                            }
                        } else if let Some(url) = source.get("url").and_then(Value::as_str) {
                            if valid_image_url(url) {
                                urls.push(url.to_owned());
                            }
                        }
                    }
                }
                object.values().for_each(|value| visit(value, urls));
            }
            _ => {}
        }
    }
    visit(body, &mut urls);
    urls
}

fn valid_image_url(url: &str) -> bool {
    !url.is_empty()
        && url.len() <= MAX_IMAGE_URL_BYTES
        && (url.starts_with("data:image/")
            || url.starts_with("https://")
            || url.starts_with("http://"))
}

/// Ask a configured vision-capable model for a compact description. The
/// endpoint is supplied by the caller so this helper never follows arbitrary
/// URLs or reaches an unconfigured provider.
pub async fn generate_descriptor(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
    model: &str,
    image_url: &str,
) -> Result<String, VisionDescriptorError> {
    generate_descriptor_with_prompt(
        client,
        endpoint,
        api_key,
        model,
        image_url,
        "Describe this image briefly and factually for another language model.",
    )
    .await
}

pub async fn generate_descriptor_with_prompt(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
    model: &str,
    image_url: &str,
    prompt: &str,
) -> Result<String, VisionDescriptorError> {
    if !valid_image_url(image_url) || model.trim().is_empty() {
        return Err(VisionDescriptorError::InvalidImageUrl);
    }
    let mut request = client.post(endpoint).json(&serde_json::json!({
        "model": model,
        "messages": [{"role":"user", "content":[
            {"type":"text", "text":prompt},
            {"type":"image_url", "image_url":{"url":image_url}}
        ]}],
        "max_tokens": 256,
        "temperature": 0
    }));
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    let payload: Value = request.send().await?.error_for_status()?.json().await?;
    let text = payload
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or(VisionDescriptorError::EmptyResponse)?;
    Ok(text.to_owned())
}

#[derive(Debug, Default)]
pub struct VisionDescriptorCache {
    entries: HashMap<String, String>,
    lru: VecDeque<String>,
}

impl VisionDescriptorCache {
    pub fn key(image_url: &str, model: &str) -> String {
        format!(
            "{model}:{}",
            hex::encode(Sha256::digest(image_url.as_bytes()))
        )
    }

    pub fn get(&mut self, image_url: &str, model: &str) -> Option<String> {
        let key = Self::key(image_url, model);
        let value = self.entries.get(&key)?.clone();
        self.touch(&key);
        Some(value)
    }

    pub fn insert(&mut self, image_url: &str, model: &str, description: String) {
        let key = Self::key(image_url, model);
        self.entries.insert(key.clone(), description);
        self.touch(&key);
        while self.lru.len() > MAX_ENTRIES {
            if let Some(oldest) = self.lru.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
    }

    fn touch(&mut self, key: &str) {
        self.lru.retain(|existing| existing != key);
        self.lru.push_back(key.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::{image_urls, VisionDescriptorCache};

    #[test]
    fn hit_miss_and_distinct_keys() {
        let mut cache = VisionDescriptorCache::default();
        cache.insert("image-a", "model-a", "description-a".into());
        assert_eq!(
            cache.get("image-a", "model-a").as_deref(),
            Some("description-a")
        );
        assert!(cache.get("image-b", "model-a").is_none());
        assert!(cache.get("image-a", "model-b").is_none());
    }

    #[test]
    fn clear_forces_recomputation() {
        let mut cache = VisionDescriptorCache::default();
        cache.insert("image-a", "model-a", "description-a".into());
        cache.clear();
        assert!(cache.get("image-a", "model-a").is_none());
    }

    #[test]
    fn evicts_oldest_entry() {
        let mut cache = VisionDescriptorCache::default();
        for index in 0..501 {
            cache.insert(&format!("image-{index}"), "model", index.to_string());
        }
        assert!(cache.get("image-0", "model").is_none());
        assert!(cache.get("image-500", "model").is_some());
    }

    #[test]
    fn extracts_only_bounded_supported_image_urls() {
        let body = serde_json::json!({"messages":[{"content":[
            {"type":"image_url","image_url":{"url":"https://example.test/a.png"}},
            {"type":"image_url","image_url":{"url":"file:///etc/passwd"}},
            {"type":"image_url","image_url":{}}
        ]}]});
        assert_eq!(image_urls(&body), vec!["https://example.test/a.png"]);
    }

    #[test]
    fn malformed_input_is_empty_not_a_panic() {
        assert!(image_urls(&serde_json::json!({"messages": null})).is_empty());
        assert!(image_urls(&serde_json::json!("not a request")).is_empty());
    }
}

#[cfg(test)]
mod upstream_tests {
    use super::*;
    use axum::{routing::post, Json, Router};

    #[test]
    fn anthropic_images_support_inline_and_url_sources() {
        let body = serde_json::json!({"messages":[{"content":[
            {"type":"image", "source":{"type":"base64", "media_type":"image/jpeg", "data":"YWJj"}},
            {"type":"image", "source":{"type":"url", "url":"https://example.test/image.png"}},
            {"type":"image", "source":{"url":"file:///private"}}
        ]}]});
        assert_eq!(
            image_urls(&body),
            vec![
                "data:image/jpeg;base64,YWJj",
                "https://example.test/image.png"
            ]
        );
    }

    #[tokio::test]
    async fn descriptor_uses_configured_upstream_auth_and_image_content() {
        let app = Router::new().route(
            "/chat/completions",
            post(
                |headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers["authorization"], "Bearer test-only");
                    assert_eq!(body["model"], "vision-model");
                    assert_eq!(
                        body["messages"][0]["content"][1]["image_url"]["url"],
                        "data:image/png;base64,YWJj"
                    );
                    Json(serde_json::json!({"choices":[{"message":{"content":" A red square. "}}]}))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = generate_descriptor(
            &reqwest::Client::new(),
            &format!("http://{addr}/chat/completions"),
            Some("test-only"),
            "vision-model",
            "data:image/png;base64,YWJj",
        )
        .await;
        server.abort();
        assert_eq!(result.unwrap(), "A red square.");
    }

    #[tokio::test]
    async fn empty_descriptor_is_an_error_not_a_cached_success() {
        let app = Router::new().route(
            "/chat/completions",
            post(|| async { Json(serde_json::json!({"choices":[]})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let result = generate_descriptor(
            &reqwest::Client::new(),
            &format!("http://{addr}/chat/completions"),
            None,
            "vision",
            "https://example.test/a.png",
        )
        .await;
        server.abort();
        assert!(matches!(result, Err(VisionDescriptorError::EmptyResponse)));
    }
}
