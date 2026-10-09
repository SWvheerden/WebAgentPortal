# Rust review checklist

Apply every rule to the diff. Report a hit only when it reaches Medium or above in the
context of this diff; a rule that would only produce a Low finding stays unreported.

## 1. Safety and soundness (highest priority)

- Every `unsafe` block carries a `// SAFETY:` comment, and the invariant it states holds.
- Undefined behaviour: raw pointer casts, `transmute`, and `UnsafeCell` use that could race.
- FFI: memory is freed by the side that allocated it; types crossing the boundary are
  `#[repr(C)]`.
- Panic safety: shared state touched inside `catch_unwind` is left consistent.

## 2. Ownership and memory

- `.clone()` / `.to_owned()` where a borrow works.
- Nested smart pointers such as `Arc<Mutex<Rc<RefCell<T>>>>`.
- Lock guards held longer than needed — scope them with `{}`.
- `Rc` / `Arc` cycles — break them with `Weak`.
- Functions take `&str` / `&[T]` unless they consume ownership; large values are passed
  by reference.

## 3. Error handling

- `.unwrap()` in non-test code, and `.expect()` without a proven invariant — propagate with
  `?`, `match`, or `if let`.
- `let _ = ...` discarding a `Result` / `Option` without handling or logging it.
- Errors carry context (`thiserror` / `anyhow`) and implement `std::error::Error`.
- `mutex.lock()` results handled, including poisoning.

## 4. Performance

- Allocation inside loops: hoist `Vec` / `String` / `HashMap` out and `.clear()` them; use
  `with_capacity` when the size is known.
- Redundant allocation in iterator chains, e.g. `.collect::<Vec<_>>().into_iter()`.
- `.unwrap_or(expensive())` where `.unwrap_or_else(|| expensive())` defers the work.
- `Box<dyn Trait>` / `&dyn Trait` where generics or `impl Trait` would do.
- `BTreeMap` / `BTreeSet` / linked lists in hot code where `Vec` / `VecDeque` fits.
- On a hot path the diff introduces: missing `#[inline]` on small cross-crate functions,
  struct padding waste, `format!` where `write!` would do, and `SeqCst` where
  `Relaxed` / `Acquire` / `Release` suffices.

## 5. Idiomatic design

- Long `if` / `else if` chains that read better as `match` or `if let`.
- Manual index loops (`for i in 0..vec.len()`) that should be iterators.
- Ad-hoc `to_my_type()` methods that should be `From` / `Into`.
- New public items that could be `pub(crate)`.

## 6. Async and concurrency

- Blocking work inside an `async fn` (`std::fs`, `std::thread::sleep`, heavy CPU) —
  offload with `tokio::task::spawn_blocking`.
- A `std::sync::MutexGuard` held across `.await` — use `tokio::sync::Mutex` if the lock
  must span it.
- Futures crossing `.await` are `Send` when run on a multi-threaded executor.
- `tokio::select!` branches are cancellation-safe and leave no half-completed state.

## 7. Tests and validation

- New behaviour and fixed bugs have tests, including edge cases.
- Tests are deterministic: no reliance on timing, ordering, or shared state.
- External input is validated, and no secrets are exposed.
