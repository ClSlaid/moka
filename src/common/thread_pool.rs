use once_cell::sync::Lazy;
use parking_lot::RwLock;
use scheduled_thread_pool::ScheduledThreadPool;
use std::{collections::HashMap, sync::Arc};

static REGISTRY: Lazy<ThreadPoolRegistry> = Lazy::new(ThreadPoolRegistry::default);

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum PoolName {
    Housekeeper,
    Invalidator,
}

impl PoolName {
    fn thread_name_template(&self) -> &'static str {
        match self {
            PoolName::Housekeeper => "moka-housekeeper-{}",
            PoolName::Invalidator => "moka-invalidator-{}",
        }
    }
}

pub(crate) struct ThreadPool {
    pub(crate) name: PoolName,
    pub(crate) pool: ScheduledThreadPool,
    // pub(crate) num_threads: usize,
}

pub(crate) struct ThreadPoolRegistry {
    pools: RwLock<HashMap<PoolName, Arc<ThreadPool>>>,
}

impl Default for ThreadPoolRegistry {
    fn default() -> Self {
        Self {
            pools: RwLock::new(HashMap::default()),
        }
    }
}

impl ThreadPoolRegistry {
    pub(crate) fn acquire_pool(name: PoolName) -> Arc<ThreadPool> {
        loop {
            {
                // Acquire a read lock and get the pool.
                let pools = REGISTRY.pools.read();
                if let Some(pool) = pools.get(&name) {
                    return Arc::clone(pool);
                }
            }
            {
                // Acquire the write lock, double check the pool still does not exist,
                // and insert a new pool.
                let mut pools = REGISTRY.pools.write();
                pools.entry(name).or_insert_with(|| {
                    let num_threads = crate::common::num_cpus();
                    let pool =
                        ScheduledThreadPool::with_name(name.thread_name_template(), num_threads);
                    let t_pool = ThreadPool {
                        name,
                        pool,
                        // num_threads,
                    };
                    Arc::new(t_pool)
                });
            }
        }
    }

    pub(crate) fn release_pool(pool: &Arc<ThreadPool>) {
        if Arc::strong_count(pool) <= 2 {
            // No other client exists; only this Arc and the registry are
            // the owners. Let's remove and drop the one in the registry.
            let name = pool.name;
            let mut pools = REGISTRY.pools.write();
            if let Some(pool) = pools.get(&name) {
                if Arc::strong_count(pool) <= 2 {
                    pools.remove(&name);
                }
            }
        }
    }
}
