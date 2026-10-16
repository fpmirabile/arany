# Ownership decisions

Decide who owns the value and how long it must live before changing its spelling to satisfy the compiler.

## Borrow, move, or clone

| Need | Starting point | Tradeoff |
| --- | --- | --- |
| Inspect caller-owned data during a call | `&T`, `&str`, `&[T]`, `&Path` | Ownership stays with the caller; a returned borrow ties lifetimes together |
| Mutate caller-owned data exclusively | `&mut T` | Exclusive access is explicit; keep the borrow's scope short and visible |
| Retain or consume data | owned `T`, `String`, `Vec<T>`, `PathBuf` | The caller moves or explicitly clones; the signature says "I keep this" |
| Keep an independent snapshot | `clone()` the needed value | Simpler than shared state; cost depends on the type's `Clone` |
| Share one allocation among owners | `Rc<T>` single-threaded, `Arc<T>` across threads | Cloning the handle shares the value; it does not copy it or allow independent mutation |

- Parameters: `&str` over `&String`, `&[T]` over `&Vec<T>`, `&Path` over `&PathBuf`; the borrowed form accepts more callers and promises less (C-GENERIC, Rust Design Patterns "borrowed types for arguments"). Take `impl AsRef<str>` or `impl Into<String>` only when callers pass mixed types routinely; a generic parameter costs a monomorphization per caller type.
- Owning parameters are right when the function stores the value. Taking `&T` and cloning inside is not wrong, but it hides the cost from the caller; prefer the owned parameter so the caller decides between move and clone (C-CALLER-CONTROL).
- A clone is worth a look when it is large, repeated on a hot path or hides who owns the data. A modest clone that decouples two lifetimes is often the clearest code. Reach for `Rc`/`Arc` when ownership really is shared, not as the first response to a borrow error.
- `mem::take(&mut field)` and `Option::take()` move a value out of a `&mut` without cloning; `mem::replace` swaps in a specific value. These solve most "cannot move out of borrowed content" errors.
- Lifetime annotations describe relationships; they never extend a value's life. A struct holding `&'a str` is a view, and the owner must outlive it; store `String` or `Arc<str>` when the struct outlives its source.

Borrow when the output is a view into the input:

```rust
fn first_word(text: &str) -> Option<&str> {
    text.split_whitespace().next()
}
```

Own when the value must outlive the call:

```rust
struct Profile { name: String }
impl Profile {
    fn new(name: String) -> Self { Self { name } }   // caller moves or clones
}
```

## `Copy`, `Cow`, and iteration

- `Copy` means implicit duplication is allowed, not that it is cheap: a `[u8; 4096]` can be `Copy`. Derive it for small plain values (ids, fieldless enums, pairs of integers) and pass those by value; pedantic `trivially_copy_pass_by_ref` and `large_types_passed_by_value` mark the two mistakes.
- `Cow<'a, str>` returns the input unchanged on the fast path and an owned value only when a transformation happened (escaping, normalizing). Its lifetime and branching must buy something over always borrowing or always owning; "not sure who owns it" is not that reason.
- `.iter()` borrows, `.iter_mut()` borrows mutably, `.into_iter()` on an owned collection consumes it; on a reference it uses the reference's impl. Edition 2024 makes `Box<[T]>::into_iter` yield `T` by value.
- Iterator chains are lazy and allocate nothing until `collect`; a `for` loop is equally fast and reads better when the body mutates, branches or has effects. `collect` only when ownership, reuse or an API requires the collection; `needless_collect` catches a collect-then-iterate.
- Prefer `ok_or_else`, `unwrap_or_else`, `map_or_else` when the fallback allocates or has effects; `ok_or`/`unwrap_or` when it is a cheap constant.

## Allocation awareness

Allocation is the usual hidden cost in Rust code that looks clean. The Performance Book's rules:

- `Vec::with_capacity(n)` or `reserve` when the size is known; growth from empty reallocates several times.
- Reuse a buffer across iterations (`buf.clear()` then refill; `read_line(&mut line)` over `lines()`), and pass `&mut Vec<T>` into a producer rather than returning many small `Vec`s to concatenate.
- `format!` allocates a `String`; `write!(buf, ...)` into an existing buffer does not. A `&'static str` literal costs nothing.
- `clone_from` reuses the destination's allocation; `to_owned()`/`to_string()`/`clone()` on `&str` all allocate; `inefficient_to_string` catches `to_string` on a `&&str`.
- `Box<[T]>` and `Box<str>` are the right shape for a fixed-size result stored for a long time; `Vec` carries a spare capacity word.
- `Arc<str>`/`Arc<[T]>` share immutable data across tasks without copying; cloning them is a refcount increment, not an allocation.
- A small enum with one huge variant makes every value huge (`large_enum_variant`); box the big one.

Measure before going further ([performance claims](verification-and-concurrency.md#measure-performance-claims)).

## Pointer semantics

`Box<T>` owns a heap allocation: recursive types, type erasure, an intentional layout or a large value moved cheaply. `&T` is a shared reference, not a promise of immutability: `Cell<T>` (`Copy` values), `RefCell<T>` (runtime-checked borrows, panics on conflict; `try_borrow` for a fallible path) and the atomics mutate through `&T`.

Thread-safety follows the contents: `&T: Send` when `T: Sync`; `&mut T: Send` when `T: Send`; `Arc<T>: Send + Sync` when `T: Send + Sync`. `Arc<Mutex<T>>` protects `T` across threads; `Arc<T>` alone protects only the reference count. `Rc`, `RefCell` and raw pointers remove `Send`/`Sync`; `std::sync::MutexGuard` is `!Send`, which is why holding one across `.await` fails to compile in a spawned task. The compiler and the exact type's documented impls are the authority.

Sources: [Book ch. 4 ownership](https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html), [`Copy`](https://doc.rust-lang.org/std/marker/trait.Copy.html), [`Cow`](https://doc.rust-lang.org/std/borrow/enum.Cow.html), [`IntoIterator`](https://doc.rust-lang.org/std/iter/trait.IntoIterator.html), [`mem::take`](https://doc.rust-lang.org/std/mem/fn.take.html), [`Sync`](https://doc.rust-lang.org/std/marker/trait.Sync.html), [`Arc`](https://doc.rust-lang.org/std/sync/struct.Arc.html), [Performance Book heap allocations](https://nnethercote.github.io/perf-book/heap-allocations.html), [Rust Design Patterns idioms](https://rust-unofficial.github.io/patterns/idioms/index.html), [Effective Rust items 8, 9, 14, 15](https://www.lurklurk.org/effective-rust/).
