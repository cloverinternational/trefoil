//! Process-local cache for generated vision descriptors.

use std::collections::{HashMap, VecDeque};

use sha2::{Digest, Sha256};

const MAX_ENTRIES: usize = 500;

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
    use super::VisionDescriptorCache;

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
}
