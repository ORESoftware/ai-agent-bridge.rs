//! Topic embeddings + cosine similarity.
//!
//! Two backends: an optional remote HTTP embedder (the in-cluster
//! `dd-embeddings-rs`, or any OpenAI-style `/embeddings` endpoint) and a
//! self-contained deterministic local embedder. The local one is the default and
//! the guaranteed fallback, so topic routing works with zero external
//! dependencies and stays reproducible in tests.

use tracing::{debug, warn};

use crate::config::{MAX_EMBEDDING_RESPONSE_BYTES, MAX_EMBED_DIM};

/// Label recorded on channels embedded by the built-in hasher. Kept honest: even
/// when a remote embedder is configured, a fallback vector is labeled local (not
/// the remote model) so an operator can detect a mixed embedding space.
pub const LOCAL_MODEL: &str = "local-hash-v1";

#[derive(Clone)]
pub struct Embedder {
    dim: usize,
    remote: Option<RemoteEmbedder>,
}

#[derive(Clone)]
struct RemoteEmbedder {
    url: String,
    model: String,
    bearer: Option<String>,
    client: reqwest::Client,
    expected_dim: usize,
    max_response_bytes: usize,
}

impl Embedder {
    pub fn new(
        dim: usize,
        url: Option<String>,
        remote_model: String,
        bearer: Option<String>,
        max_response_bytes: usize,
    ) -> Self {
        let dim = dim.clamp(1, MAX_EMBED_DIM);
        let remote = url.map(|url| RemoteEmbedder {
            url,
            model: remote_model,
            bearer,
            expected_dim: dim,
            max_response_bytes: max_response_bytes.clamp(1, MAX_EMBEDDING_RESPONSE_BYTES),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
        });
        Self { dim, remote }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Embed text, preferring the remote backend and falling back to local on any
    /// error. Returns the vector **and the model that actually produced it**, so
    /// the channel records the truth even after a remote outage.
    pub async fn embed(&self, text: &str) -> (Vec<f32>, String) {
        // Cap the embedded prefix: topic routing gains nothing from megabytes of
        // input, and the local hasher's per-trigram work would otherwise stall the
        // async worker on a hostile query. 16 KiB is far more than any real topic.
        const MAX_EMBED_BYTES: usize = 16 * 1024;
        let text = if text.len() > MAX_EMBED_BYTES {
            let mut end = MAX_EMBED_BYTES;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        } else {
            text
        };
        if let Some(remote) = &self.remote {
            match remote.embed(text).await {
                Ok(v) => {
                    debug!(dim = v.len(), "remote embedding");
                    return (normalize(v), remote.model.clone());
                }
                Err(e) => warn!(error = %e, "remote embedder failed; using local fallback"),
            }
        }
        (local_embed(text, self.dim), LOCAL_MODEL.to_string())
    }
}

impl RemoteEmbedder {
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let body = serde_json::json!({ "input": text, "model": self.model });
        let mut req = self.client.post(&self.url).json(&body);
        if let Some(b) = &self.bearer {
            req = req.bearer_auth(b);
        }
        let mut response = req.send().await?.error_for_status()?;
        if response
            .content_length()
            .is_some_and(|length| length > self.max_response_bytes as u64)
        {
            anyhow::bail!(
                "embedding response exceeds {} bytes",
                self.max_response_bytes
            );
        }
        let capacity = response
            .content_length()
            .unwrap_or(0)
            .min(self.max_response_bytes as u64) as usize;
        let mut bytes = Vec::with_capacity(capacity);
        while let Some(chunk) = response.chunk().await? {
            let next_len = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or_else(|| anyhow::anyhow!("embedding response size overflow"))?;
            if next_len > self.max_response_bytes {
                anyhow::bail!(
                    "embedding response exceeds {} bytes",
                    self.max_response_bytes
                );
            }
            bytes.extend_from_slice(&chunk);
        }
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        extract_embedding(&json, self.expected_dim).ok_or_else(|| {
            anyhow::anyhow!(
                "embedding response must contain exactly {} finite values",
                self.expected_dim
            )
        })
    }
}

/// Pull a vector out of the common embedding-response shapes:
/// OpenAI (`data[0].embedding`), `{embedding:[…]}`, or `{embeddings:[[…]]}`.
fn extract_embedding(json: &serde_json::Value, expected_dim: usize) -> Option<Vec<f32>> {
    // Reject the whole vector (-> local fallback) if any element is non-numeric
    // or non-finite, rather than silently dropping/poisoning it: a malformed
    // element must not shrink the vector or feed NaN/inf into cosine.
    let as_vec = |v: &serde_json::Value| -> Option<Vec<f32>> {
        let arr = v.as_array()?;
        if arr.len() != expected_dim || arr.len() > MAX_EMBED_DIM {
            return None;
        }
        let mut out = Vec::with_capacity(arr.len());
        for x in arr {
            let f = x.as_f64()? as f32;
            if !f.is_finite() {
                return None;
            }
            out.push(f);
        }
        Some(out)
    };
    if let Some(v) = json.get("embedding").and_then(as_vec) {
        return Some(v);
    }
    if let Some(v) = json
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|e| e.get("embedding"))
        .and_then(as_vec)
    {
        return Some(v);
    }
    if let Some(v) = json
        .get("embeddings")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(as_vec)
    {
        return Some(v);
    }
    None
}

/// Deterministic bag-of-features embedding: hashes word tokens and character
/// 3-grams into `dim` buckets, then L2-normalizes. No dependencies, no network,
/// identical output for identical input — good enough to cluster related topics
/// and fully reproducible for tests.
pub fn local_embed(text: &str, dim: usize) -> Vec<f32> {
    let dim = dim.max(1);
    let mut v = vec![0f32; dim];
    let lower = text.to_lowercase();

    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();

    for tok in &tokens {
        let h = fnv1a(tok.as_bytes());
        let idx = (h % dim as u64) as usize;
        let sign = if (h >> 63) & 1 == 1 { -1.0 } else { 1.0 };
        v[idx] += sign;
    }

    // Character 3-grams add robustness to word-boundary and morphology noise
    // (e.g. "deploy" vs "deployment" share most trigrams).
    let chars: Vec<char> = lower
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ')
        .collect();
    for window in chars.windows(3) {
        let gram: String = window.iter().collect();
        let h = fnv1a(gram.as_bytes());
        let idx = (h % dim as u64) as usize;
        v[idx] += 0.5;
    }

    normalize(v)
}

pub fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

/// Cosine similarity. Vectors of differing lengths compare over their shared
/// prefix (defensive; in practice all channel vectors share one width).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn serve_json(body: serde_json::Value) -> String {
        use axum::{routing::post, Router};
        use std::sync::Arc;

        let body = Arc::new(body.to_string());
        let app = Router::new().route(
            "/",
            post(move || {
                let body = body.clone();
                async move {
                    (
                        [("content-type", "application/json")],
                        body.as_str().to_owned(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{address}/")
    }

    #[test]
    fn local_embed_is_deterministic() {
        let a = local_embed("deploy the soccer policy to kubernetes", 256);
        let b = local_embed("deploy the soccer policy to kubernetes", 256);
        assert_eq!(a, b);
        assert_eq!(a.len(), 256);
    }

    #[test]
    fn normalized_vectors_are_unit_length() {
        let v = local_embed("hello world", 128);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "norm was {norm}");
    }

    #[test]
    fn cosine_ranks_related_topics_above_unrelated() {
        let query = local_embed("kubernetes deployment rollout", 256);
        let related = local_embed("deploying services to a kubernetes cluster", 256);
        let unrelated = local_embed("baking sourdough bread at home", 256);
        let s_rel = cosine(&query, &related);
        let s_unrel = cosine(&query, &unrelated);
        assert!(
            s_rel > s_unrel,
            "related {s_rel} should beat unrelated {s_unrel}"
        );
    }

    #[test]
    fn cosine_of_identical_is_one() {
        let v = local_embed("same text", 64);
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn extract_handles_openai_and_flat_shapes() {
        let openai = serde_json::json!({ "data": [ { "embedding": [0.1, 0.2, 0.3] } ] });
        assert_eq!(extract_embedding(&openai, 3), Some(vec![0.1, 0.2, 0.3]));
        let flat = serde_json::json!({ "embedding": [1.0, 2.0] });
        assert_eq!(extract_embedding(&flat, 2), Some(vec![1.0, 2.0]));
        let nested = serde_json::json!({ "embeddings": [[4.0, 5.0]] });
        assert_eq!(extract_embedding(&nested, 2), Some(vec![4.0, 5.0]));
        let none = serde_json::json!({ "nope": true });
        assert_eq!(extract_embedding(&none, 2), None);
        // A non-numeric element rejects the whole vector (-> local fallback),
        // rather than silently dropping it and shrinking the dimension.
        let bad = serde_json::json!({ "embedding": [0.1, "x", 0.2] });
        assert_eq!(extract_embedding(&bad, 3), None);
        assert_eq!(extract_embedding(&flat, 3), None);
    }

    #[tokio::test]
    async fn remote_response_size_and_dimension_are_bounded() {
        let valid_url = serve_json(serde_json::json!({"embedding":[1.0, 2.0, 3.0]})).await;
        let valid = Embedder::new(3, Some(valid_url), "remote-v1".into(), None, 1024);
        let (vector, model) = valid.embed("topic").await;
        assert_eq!(vector.len(), 3);
        assert_eq!(model, "remote-v1");

        let wrong_dim_url = serve_json(serde_json::json!({"embedding":[1.0, 2.0, 3.0, 4.0]})).await;
        let wrong_dim = Embedder::new(3, Some(wrong_dim_url), "remote-v1".into(), None, 1024);
        let (vector, model) = wrong_dim.embed("topic").await;
        assert_eq!(vector.len(), 3);
        assert_eq!(model, LOCAL_MODEL);

        let oversized_url = serve_json(serde_json::json!({
            "embedding":[1.0, 2.0, 3.0],
            "padding":"x".repeat(2048)
        }))
        .await;
        let oversized = Embedder::new(3, Some(oversized_url), "remote-v1".into(), None, 64);
        let (vector, model) = oversized.embed("topic").await;
        assert_eq!(vector.len(), 3);
        assert_eq!(model, LOCAL_MODEL);
    }

    #[test]
    fn configured_dimension_is_clamped_to_the_absolute_maximum() {
        let embedder = Embedder::new(usize::MAX, None, "local".into(), None, 1024);
        assert_eq!(embedder.dim(), MAX_EMBED_DIM);
    }
}
