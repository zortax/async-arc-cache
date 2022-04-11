use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::oneshot::error::RecvError;
use tokio::sync::oneshot::{channel, Sender};

#[derive(Error, Debug)]
pub enum CacheError {
  #[error("This cache has been dropped")]
  CacheClosed,

  #[error(transparent)]
  FailedToGet(#[from] RecvError),
}

enum CacheCommand<K: Eq + Hash + 'static, V> {
  GetItem(K, Sender<CachedArc<K, V>>),
  GetItemIfExists(K, Sender<Option<CachedArc<K, V>>>),
  DeleteItem(Arc<K>),
  Exit,
}

pub struct CachedArc<K: Eq + Hash + 'static, V> {
  key: Arc<K>,
  inner: Arc<V>,
  rc: Arc<AtomicUsize>,
  sender: UnboundedSender<CacheCommand<K, V>>,
}

impl<K: Eq + Hash + 'static, V> Deref for CachedArc<K, V> {
  type Target = V;

  fn deref(&self) -> &Self::Target {
    self.inner.deref()
  }
}

impl<K: Eq + Hash + 'static, V> Clone for CachedArc<K, V> {
  fn clone(&self) -> Self {
    self.rc.fetch_add(1, Ordering::Relaxed);
    CachedArc {
      key: self.key.clone(),
      inner: self.inner.clone(),
      rc: self.rc.clone(),
      sender: self.sender.clone(),
    }
  }
}

impl<K: Eq + Hash + 'static, V> Drop for CachedArc<K, V> {
  fn drop(&mut self) {
    if self.rc.fetch_sub(1, Ordering::Relaxed) == 2 && !self.sender.is_closed()
    {
      let _ignored =
        self.sender.send(CacheCommand::DeleteItem(self.key.clone()));
    }
  }
}

impl<K: Eq + Hash + 'static, V> CachedArc<K, V> {
  pub fn inner(&self) -> Arc<V> {
    self.inner.clone()
  }

  pub fn ref_count(&self) -> usize {
    self.rc.load(Ordering::Relaxed)
  }
}

pub struct ArcCache<K: Eq + Hash + 'static, V> {
  sender: UnboundedSender<CacheCommand<K, V>>,
}

impl<K: Eq + Hash + 'static, V> ArcCache<K, V> {
  pub fn with_constructor<F, Fut>(
    constructor: F,
  ) -> (Self, impl Future<Output = ()>)
  where
    F: Fn(&'static K) -> Fut,
    Fut: Future<Output = V>,
  {
    let (sender, mut receiver) = unbounded_channel::<CacheCommand<K, V>>();
    let fut = {
      let sender = sender.clone();
      async move {
        let mut data: HashMap<Arc<K>, CachedArc<K, V>> = HashMap::new();

        while let Some(command) = receiver.recv().await {
          match command {
            CacheCommand::GetItem(key, responder) => {
              let result = if let Some(value) = data.get(&key) {
                (*value).clone()
              } else {
                // SAFETY: safe, as we fully await the future this reference is
                // moved to before moving/dropping the value it points to
                let key_ref: &'static K = unsafe { std::mem::transmute(&key) };
                let inner = constructor(key_ref).await;
                let key = Arc::new(key);
                let value = CachedArc {
                  key: key.clone(),
                  inner: Arc::new(inner),
                  rc: Arc::new(AtomicUsize::new(1)),
                  sender: sender.clone(),
                };
                let _ignored = data.insert(key, value.clone());
                value
              };
              let _ignored = responder.send(result);
            }
            CacheCommand::GetItemIfExists(key, responder) => {
              let _ignored = responder.send(if data.contains_key(&key) {
                let result = if let Some(value) = data.get(&key) {
                  (*value).clone()
                } else {
                  // SAFETY: safe, as we fully await the future this reference
                  // is moved to before moving/dropping the
                  // value it points to
                  let key_ref: &'static K =
                    unsafe { std::mem::transmute(&key) };
                  let inner = constructor(key_ref).await;
                  let key = Arc::new(key);
                  let value = CachedArc {
                    key: key.clone(),
                    inner: Arc::new(inner),
                    rc: Arc::new(AtomicUsize::new(1)),
                    sender: sender.clone(),
                  };
                  let _ignored = data.insert(key, value.clone());
                  value
                };
                Some(result)
              } else {
                None
              });
            }
            CacheCommand::DeleteItem(key) => {
              let _ignored = data.remove(key.deref());
            }
            CacheCommand::Exit => {
              break;
            }
          }
        }
        receiver.close();
      }
    };
    (Self { sender }, fut)
  }

  pub fn with_default_constructor() -> (Self, impl Future<Output = ()>)
  where
    V: Default,
  {
    Self::with_constructor(|_| async { V::default() })
  }

  pub async fn get(&self, key: K) -> Result<CachedArc<K, V>, CacheError> {
    if self.sender.is_closed() {
      return Err(CacheError::CacheClosed);
    }
    let (responder, response) = channel();
    self
      .sender
      .send(CacheCommand::GetItem(key, responder))
      .map_err(|_| CacheError::CacheClosed)?;
    Ok(response.await?)
  }

  pub async fn get_if_exists(
    &self,
    key: K,
  ) -> Result<Option<CachedArc<K, V>>, CacheError> {
    if self.sender.is_closed() {
      return Err(CacheError::CacheClosed);
    }
    let (responder, response) = channel();
    self
      .sender
      .send(CacheCommand::GetItemIfExists(key, responder))
      .map_err(|_| CacheError::CacheClosed)?;
    Ok(response.await?)
  }
}

impl<K: Eq + Hash, V> Drop for ArcCache<K, V> {
  fn drop(&mut self) {
    let _ignored = self.sender.send(CacheCommand::Exit);
  }
}

#[cfg(test)]
mod tests {
  use std::ops::Deref;
  use std::sync::atomic::{AtomicI32, Ordering};

  use crate::ArcCache;

  #[tokio::test]
  async fn test_cache() {
    let (cache, controller) =
      ArcCache::<i32, i32>::with_constructor(|key| async move {
        static COUNTER: AtomicI32 = AtomicI32::new(10);
        key * COUNTER.fetch_add(1, Ordering::Relaxed)
      });
    tokio::spawn(controller);

    let cached_arc = cache.get(10).await.unwrap();
    assert_eq!(cached_arc.deref().clone(), 100);

    let cached_arc2 = cache.get(10).await.unwrap();
    assert_eq!(cached_arc2.deref().clone(), 100);

    drop(cached_arc);
    drop(cached_arc2);
    assert_eq!(cache.get(10).await.unwrap().deref().clone(), 110);
  }
}
