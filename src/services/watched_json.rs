use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use serde::de::DeserializeOwned;
use tokio::{task::JoinHandle, time};

#[derive(Debug, thiserror::Error)]
pub enum WatchedJsonError {
    #[error("failed to read config file: {0}")]
    ReadFile(#[from] std::io::Error),
    #[error("failed to parse config JSON: {0}")]
    ParseJson(#[from] serde_json::Error),
    #[error("failed to read config: {0}")]
    ReadLock(String),
}

/// A JSON file deserialized into [T] and re-read every 250ms, so an edit on
/// the host is live without a restart. A file that stops parsing keeps the
/// last good value and warns once. A missing or invalid file at startup is an
/// error, so a bad deploy fails loudly.
#[derive(Debug)]
pub struct WatchedJson<T> {
    value: Arc<RwLock<T>>,
    watch_task: JoinHandle<()>,
}

impl<T> WatchedJson<T>
where
    T: DeserializeOwned + PartialEq + Send + Sync + 'static,
{
    const POLL_INTERVAL: Duration = Duration::from_millis(250);

    pub fn new(file_path: impl Into<PathBuf>) -> Result<Self, WatchedJsonError> {
        let file_path = file_path.into();
        let value = Arc::new(RwLock::new(Self::load_sync(&file_path)?));
        let shared = value.clone();

        let watch_task = tokio::spawn(async move {
            let mut interval = time::interval(Self::POLL_INTERVAL);
            interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
            let mut last_reload_failed = false;

            loop {
                interval.tick().await;

                match Self::load_async(&file_path).await {
                    Ok(updated) => {
                        last_reload_failed = false;
                        if let Ok(mut guard) = shared.write() {
                            if *guard != updated {
                                *guard = updated;
                                tracing::info!("Reloaded {}", file_path.display());
                            }
                        }
                    }
                    Err(err) => {
                        if !last_reload_failed {
                            tracing::warn!(
                                "Failed to reload {}: {}. Using last known good value.",
                                file_path.display(),
                                err
                            );
                            last_reload_failed = true;
                        }
                    }
                }
            }
        });

        Ok(Self { value, watch_task })
    }

    pub fn get(&self) -> Result<T, WatchedJsonError>
    where
        T: Clone,
    {
        self.with(T::clone)
    }

    /// Runs [f] on the current value without cloning it.
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> Result<R, WatchedJsonError> {
        let guard = self
            .value
            .read()
            .map_err(|_| WatchedJsonError::ReadLock("lock poisoned".to_string()))?;
        Ok(f(&guard))
    }

    fn load_sync(path: &Path) -> Result<T, WatchedJsonError> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }

    async fn load_async(path: &Path) -> Result<T, WatchedJsonError> {
        Ok(serde_json::from_slice(&tokio::fs::read(path).await?)?)
    }
}

impl<T> Drop for WatchedJson<T> {
    fn drop(&mut self) {
        self.watch_task.abort();
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    use uuid::Uuid;

    /// Writes [contents] to a fresh file under target/test-tmp and returns its path.
    pub fn write_temp_json(name: &str, contents: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("watched-json-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write json");
        path
    }
}
