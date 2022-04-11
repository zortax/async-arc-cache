async-arc-cache
--
A simple, async cache that caches values as long as there is at least one reference to them.
```rust
let (cache, fut) =
  ArcCache::<i32, i32>::with_constructor(|key| async move {
    static COUNTER: AtomicI32 = AtomicI32::new(10);
    key * COUNTER.fetch_add(1, Ordering::Relaxed)
  });
tokio::spawn(fut);

let cached_arc = cache.get(10).await.unwrap();
assert_eq!(cached_arc.deref().clone(), 100);

let cached_arc2 = cache.get(10).await.unwrap();
assert_eq!(cached_arc2.deref().clone(), 100);

drop(cached_arc);
drop(cached_arc2);
assert_eq!(cache.get(10).await.unwrap().deref().clone(), 110);
```
