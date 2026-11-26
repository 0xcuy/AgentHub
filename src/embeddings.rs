//! Embedding providers for the optional vector layer on top of FTS5 recall.
//!
//! FTS5 is always on and needs nothing. Vector search is opt-in: it only
//! activates when a provider with a key is configured via the environment
//! (`AGHUB_EMBEDDINGS_API_KEY` or `MISTRAL_API_KEY`). Without a key the
//! `Noop` provider is used, the repo runs with zero external services, and
//! no key is ever committed.

use std::future::Future;
use std::pin::Pin;

pub type EmbedResult = Result<Vec<Vec<f32>>, String>;

pub type EmbedFuture = Pin<Box<dyn Future<Output = EmbedResult> + Send>>;

pub trait EmbeddingProvider: Send + Sync {
    /// Stable provider name, surfaced in logs and diagnostics.
    fn name(&self) -> &'static str;
    /// True when the provider can actually produce vectors.
    fn enabled(&self) -> bool;
    /// Embed a batch of texts, preserving input order.
    fn embed(&self, texts: Vec<String>) -> EmbedFuture;
}

/// Default provider: vector layer off, FTS5-only recall.
pub struct NoopEmbeddings;

impl EmbeddingProvider for NoopEmbeddings {
    fn name(&self) -> &'static str {
        "noop"
    }
    fn enabled(&self) -> bool {
        false
    }
    fn embed(&self, _texts: Vec<String>) -> EmbedFuture {
        Box::pin(async { Err("embedding provider is disabled".into()) })
    }
}

/// HTTP provider for the Mistral embeddings API. The key comes only from the
/// environment; no key means the provider is never constructed.
pub struct MistralEmbeddings {
    pub client: reqwest::Client,
    pub url: String,
    pub key: String,
    pub model: String,
}

impl MistralEmbeddings {
    fn from_env(http: reqwest::Client) -> Option<Self> {
        let key = std::env::var("AGHUB_EMBEDDINGS_API_KEY")
            .or_else(|_| std::env::var("MISTRAL_API_KEY"))
            .ok()?;
        let url = std::env::var("AGHUB_EMBEDDINGS_URL")
            .unwrap_or_else(|_| "https://api.mistral.ai/v1/embeddings".to_string());
        let model = std::env::var("AGHUB_EMBEDDINGS_MODEL")
            .unwrap_or_else(|_| "mistral-embed".to_string());
        Some(Self { client: http, url, key, model })
    }
}

impl EmbeddingProvider for MistralEmbeddings {
    fn name(&self) -> &'static str {
        "mistral"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn embed(&self, texts: Vec<String>) -> EmbedFuture {
        let client = self.client.clone();
        let url = self.url.clone();
        let key = self.key.clone();
        let model = self.model.clone();
        Box::pin(async move {
            let body = serde_json::json!({ "model": model, "input": texts, "encoding_format": "float" });
            let resp = client
                .post(&url)
                .bearer_auth(&key)
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("embeddings request failed: {e}"))?;
            let status = resp.status();
            let payload: serde_json::Value =
                resp.json().await.map_err(|e| format!("embeddings decode failed: {e}"))?;
            if !status.is_success() {
                return Err(format!("embeddings API returned {status}: {payload}"));
            }
            let mut out: Vec<(usize, Vec<f32>)> = Vec::new();
            let items = payload["data"]
                .as_array()
                .ok_or_else(|| "embeddings response missing data".to_string())?;
            for item in items {
                let index = item["index"].as_u64().unwrap_or(0) as usize;
                let vector = item["embedding"]
                    .as_array()
                    .ok_or_else(|| "embeddings response missing vector".to_string())?
                    .iter()
                    .filter_map(|v| v.as_f64().map(|f| f as f32))
                    .collect();
                out.push((index, vector));
            }
            out.sort_by_key(|(i, _)| *i);
            Ok(out.into_iter().map(|(_, v)| v).collect())
        })
    }
}

/// Deterministic hash-based provider for tests. Not a real model: it builds a
/// bag-of-token-hashes vector, so shared tokens produce similar vectors.
pub struct StubEmbeddings {
    pub dim: usize,
}

impl StubEmbeddings {
    fn vector(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dim];
        for token in text.to_lowercase().split_whitespace() {
            let h = fnv1a(token) as usize % self.dim.max(1);
            v[h] += 1.0;
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl EmbeddingProvider for StubEmbeddings {
    fn name(&self) -> &'static str {
        "stub"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn embed(&self, texts: Vec<String>) -> EmbedFuture {
        let stub = StubEmbeddings { dim: self.dim };
        Box::pin(async move { Ok(texts.iter().map(|t| stub.vector(t)).collect()) })
    }
}

/// Build the provider from the environment. No key -> Noop (vector off).
pub fn from_env(http: reqwest::Client) -> std::sync::Arc<dyn EmbeddingProvider> {
    match MistralEmbeddings::from_env(http) {
        Some(p) => {
            tracing::info!(model = %p.model, "vector search enabled via mistral embeddings");
            std::sync::Arc::new(p)
        }
        None => std::sync::Arc::new(NoopEmbeddings),
    }
}

/// Cosine similarity; None on dimension mismatch or a zero vector.
pub fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        let a = [1.0, 0.0];
        assert!((cosine(&a, &[1.0, 0.0]).unwrap() - 1.0).abs() < 1e-6);
        assert!((cosine(&a, &[-1.0, 0.0]).unwrap() + 1.0).abs() < 1e-6);
        assert!((cosine(&a, &[1.0, 1.0]).unwrap() - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert!(cosine(&a, &[1.0]).is_none(), "dimension mismatch");
        assert!(cosine(&a, &[0.0, 0.0]).is_none(), "zero vector");
    }

    #[test]
    fn blob_roundtrip() {
        let v = [0.25f32, -1.5, 3.0e-7, 0.0];
        let blob = vec_to_blob(&v);
        assert_eq!(blob.len(), 16);
        let back = blob_to_vec(&blob);
        assert_eq!(back.len(), 4);
        for (x, y) in v.iter().zip(back.iter()) {
            assert!((x - y).abs() < 1e-9);
        }
    }

    #[test]
    fn stub_is_deterministic_and_shared_tokens_correlate() {
        let s = StubEmbeddings { dim: 64 };
        let out = s.vector("suka kopi");
        assert_eq!(out.len(), 64);
        assert_eq!(out, s.vector("suka kopi"), "stub must be deterministic");
        let same = s.vector("kopi");
        assert!(cosine(&out, &same).unwrap() > 0.6, "shared token must correlate: {:?}", cosine(&out, &same));
        let other = s.vector("teh hijau dingin");
        assert!(cosine(&out, &other).unwrap_or(0.0) < 0.5, "disjoint tokens must not correlate");
        let res = futures_util::FutureExt::now_or_never(s.embed(vec!["a".into(), "b".into()])).unwrap().unwrap();
        assert_eq!(res.len(), 2, "batch embedding preserves input order");
    }

    #[test]
    fn noop_provider_is_disabled() {
        let p = NoopEmbeddings;
        assert!(!p.enabled());
        let out = p.embed(vec!["x".into()]);
        let res = futures_util::FutureExt::now_or_never(out).unwrap();
        assert!(res.is_err());
    }
}
