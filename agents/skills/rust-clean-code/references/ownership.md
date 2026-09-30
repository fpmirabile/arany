# Ownership decisions

Choose who owns the value and how long it must live before changing its spelling to satisfy the compiler.

## Borrow, move, or clone

| Need | Useful starting point | Tradeoff |
| --- | --- | --- |
| Inspect caller-owned data during a call | `&T`, `&str`, `&[T]` | Keeps ownership with the caller; returned borrows connect lifetimes. |
| Mutate caller-owned data exclusively | `&mut T` | Makes exclusive access explicit; keep the borrow's scope understandable. |
| Retain or consume data independently | Owned `T`, `String`, `Vec<T>` | Caller can move existing data or explicitly clone when it still needs it. |
| Keep an independent snapshot | Clone the needed value | Often simpler than shared state; cost and snapshot semantics depend on the type's `Clone` implementation. |
| Share one allocation among owners | `Rc<T>` locally or `Arc<T>` across threads when bounds permit | Cloning the handle shares the value; it does not copy its contents or provide independent mutation. |

For read-only views, slices and `&str` usually express fewer requirements than `&Vec<T>` and `&String`. Container-specific operations can justify the latter. Owning parameters are appropriate when retaining data; accepting a borrow and cloning internally is not automatically wrong, but should fit the API's contract and caller ergonomics.

A clone is worth investigating when it is large, repeated in a hot path, or obscures ownership. A modest clone can be the clearest way to decouple lifetimes. Lifetime annotations describe relationships; they do not extend a value's life. Reach for shared ownership when ownership is actually shared, rather than as the first response to a borrow-checker error.

This function borrows because its output is a view into the input:

```rust
fn first_word(text: &str) -> Option<&str> {
    text.split_whitespace().next()
}

let text = String::from("hello Rust");
assert_eq!(first_word(&text), Some("hello"));
assert_eq!(text, "hello Rust");
```

Here an owned snapshot remains independent of subsequent changes:

```rust
let mut current = String::from("draft");
let snapshot = current.clone();
current.push_str(" revised");
assert_eq!(snapshot, "draft");
assert_eq!(current, "draft revised");
```

## `Copy`, `Cow`, and iteration

- Pass simple scalar values by value when that gives a natural API. `Copy` means implicit duplication is permitted, not that any implementing type is cheap: large arrays can implement it. Choose by semantics and measured cost rather than a universal byte threshold. Deriving `Copy` for a public type is an API commitment.
- `Cow<'a, str>` fits an operation that can return borrowed text unchanged but sometimes needs an owned transformation. Its extra lifetime and branching complexity should buy something over consistently borrowing or owning. Ownership uncertainty alone is not that reason.
- For standard collections, `.iter()` borrows elements, `.iter_mut()` borrows them mutably, and calling `.into_iter()` on an owned collection consumes it. Calling `into_iter` on a reference uses that reference's implementation. Choose based on ownership and the actual type, not whether the elements implement `Copy`.
- Use iterator chains when transformations read clearly, and `for` loops when mutation, branching, or side effects read more clearly. Neither is inherently faster. Materialize a collection when its ownership, reuse, or an API requires it.
- Prefer lazy fallbacks such as `ok_or_else` when constructing the fallback is expensive or has effects that should happen only on failure. A cheap value often reads better with `ok_or`.

## Pointer semantics

`Box<T>` owns a heap allocation; it can serve recursive types, type erasure, or an intentional storage layout. Its usefulness is independent of whether a value sits at a public API boundary. Arrays live wherever their containing value lives; being an array does not require stack storage.

`&T` is a shared reference, not a guarantee that the underlying value never changes: interior-mutability types expose controlled mutation through shared references. `RefCell<T>` checks borrowing at runtime and can panic on conflicting borrows; `try_borrow` variants allow handling failure. `Cell<T>` supports some non-`Copy` operations, while `get` requires `T: Copy`.

Thread-safety claims depend on the contained type. For the standard default-allocator forms, `&T` is `Send` when `T: Sync`; `&mut T` is `Send` when `T: Send`; `Arc<T>` is `Send + Sync` when `T: Send + Sync`. An `Arc` protects the reference count, not arbitrary mutation of its contents. The compiler and the exact type's documented implementations are the authority.

Sources: [ownership and borrowing](https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html), [Copy](https://doc.rust-lang.org/std/marker/trait.Copy.html), [Cow](https://doc.rust-lang.org/std/borrow/enum.Cow.html), [IntoIterator](https://doc.rust-lang.org/std/iter/trait.IntoIterator.html), [Cell](https://doc.rust-lang.org/std/cell/struct.Cell.html), [Sync](https://doc.rust-lang.org/std/marker/trait.Sync.html), and [Arc](https://doc.rust-lang.org/std/sync/struct.Arc.html).
