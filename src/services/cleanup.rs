use crate::adapters::cache::apt_cache::AptCacheAdapter;
use crate::adapters::cache::cargo_cache::CargoCacheAdapter;
use crate::adapters::cache::conda_cache::CondaCacheAdapter;
use crate::adapters::cache::docker_cache::DockerCacheAdapter;
use crate::adapters::cache::journal_cache::JournalCacheAdapter;
use crate::adapters::cache::log_cache::LogCacheAdapter;
use crate::adapters::cache::npm_cache::NpmCacheAdapter;
use crate::adapters::cache::pip_cache::PipCacheAdapter;
use crate::adapters::cache::snap_cache::SnapCacheAdapter;
use crate::adapters::CacheAdapter;
use crate::models::CleanupSuggestion;
use tokio::task::JoinSet;

pub struct CleanupEvent {
    pub scan_id: u64,
    pub total_sources: usize,
    pub source: String,
    pub suggestions: Vec<CleanupSuggestion>,
}

fn default_adapters() -> Vec<Box<dyn CacheAdapterBoxed>> {
    vec![
        Box::new(AptCacheAdapter),
        Box::new(PipCacheAdapter),
        Box::new(NpmCacheAdapter),
        Box::new(CondaCacheAdapter),
        Box::new(CargoCacheAdapter),
        Box::new(DockerCacheAdapter),
        Box::new(JournalCacheAdapter),
        Box::new(LogCacheAdapter),
        Box::new(SnapCacheAdapter),
    ]
}

pub async fn scan_all(
    tx: async_channel::Sender<CleanupEvent>,
    token: tokio_util::sync::CancellationToken,
    scan_id: u64,
) {
    let adapters = default_adapters();
    let total_sources = adapters.len();

    let mut tasks = JoinSet::new();

    for adapter in adapters {
        if token.is_cancelled() {
            tasks.abort_all();
            return;
        }

        let name = adapter.name().to_string();
        tasks.spawn(async move {
            tracing::info!("scanning cleanup suggestions: {name}");
            let suggestions = adapter.suggest_cleanups_boxed().await;
            (name, suggestions)
        });
    }

    while let Some(joined) = tasks.join_next().await {
        if token.is_cancelled() {
            tasks.abort_all();
            return;
        }

        let (source, suggestions) = match joined {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("cleanup scan join failed: {e}");
                continue;
            }
        };

        if token.is_cancelled() {
            tasks.abort_all();
            return;
        }

        let event = CleanupEvent {
            scan_id,
            total_sources,
            source,
            suggestions,
        };

        if tx.send(event).await.is_err() {
            return;
        }
    }
}

trait CacheAdapterBoxed: Send + Sync {
    fn name(&self) -> &str;
    fn suggest_cleanups_boxed(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<CleanupSuggestion>> + Send + '_>>;
}

impl<T: CacheAdapter> CacheAdapterBoxed for T {
    fn name(&self) -> &str {
        CacheAdapter::name(self)
    }
    fn suggest_cleanups_boxed(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<CleanupSuggestion>> + Send + '_>>
    {
        Box::pin(self.suggest_cleanups())
    }
}
