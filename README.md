# babeldiff

`babeldiff` is a diff tool for reviewing changes that port C++ to Rust. It was
built for the Zircon kernel's in-place conversion, where each change replaces
a C++ implementation with Rust (called through FFI) that is meant to follow
the C++ line for line.

It parses both languages, pairs each removed C++ function with the Rust
function that replaces it, lines the two up side by side, and flags places
where the Rust does not do the same thing:

- **comments** that were lost, added, or reworded
- **error returns** that differ in code or order, and failures that one side
  handles and the other propagates
- **locks** taken in a different place or on a different lock
- **control flow** (`if`, loops, `switch`/`match`, `return`, `break`) with no
  counterpart
- **steps** (calls) that only one side performs

The output is plain text in the style of `diff -y`, meant to be read by people
and by agents.

## Install

```sh
cargo install --path .
```

This builds a single static binary; tree-sitter grammars for C++ and Rust are
compiled in.

## Usage

```sh
# In a Fuchsia checkout: compare a commit with its parent, or a range.
babeldiff git HEAD
babeldiff git origin/main..my-branch

# A patch file (git format-patch, git show, or git diff output), or stdin.
babeldiff patch 1835293.patch
git show HEAD | babeldiff patch

# With -C, patch mode reads full files from the repository by the blob ids in
# the patch, and can look up C++ that the change did not touch.
babeldiff patch -C ~/fuchsia --base origin/main 1835293.patch

# Compare every function in a set of files.
babeldiff files handle_table.cc handle_table.rs
```

Useful options:

| Option | Effect |
| --- | --- |
| `--layout stacked` | Print each C++ unit above the Rust it aligns with, untruncated. Better for agents and narrow terminals. |
| `--summary` | Only the per-function summaries and findings. |
| `-U N`, `--context N` | Only rows within N rows of a difference. |
| `--width N` | Width of the side-by-side view (default `$COLUMNS` or 160). |
| `--pair Cpp=rust` | Force a pairing the matcher missed. Repeatable. |
| `--min-changed F` | Fraction of a function's lines a change must touch for it to be compared (default 0.25). |
| `--no-search` | Don't search the repository for unchanged C++. |

The exit status is 0 when there are no issues, 1 when there are, and 2 on
error, like `diff`.

## Reading the output

```
==== FifoDispatcher::WriteFromUser  <->  FifoDispatcher::write_from_user
  C++   zircon/kernel/object/fifo_dispatcher.cc:14-23
  Rust  zircon/kernel/object/fifo_dispatcher.rs:13-29
  via   C++ now calls FFI shim rust_fifo_dispatcher_write_from_user  zircon/kernel/object/fifo_dispatcher_ffi.rs:11-30
  note  C++ lines marked * are the declaration's comment in zircon/kernel/object/include/object/fifo_dispatcher.h
  similarity 0.95, 1 issue, 1 note

   17* // Writes |count| elements of |elem_size| bytes from |ptr| into the pee… =     13 /// Writes `count` elements of `elem_size` bytes from `ptr` into the pe…
   19* // Returns ZX_ERR_OUT_OF_RANGE if |elem_size| does not match the fifo.   =     15 /// Returns ZX_ERR_OUT_OF_RANGE if `elem_size` does not match the fifo.
    14 zx_status_t FifoDispatcher::WriteFromUser(size_t elem_size, user_in_ptr… =     16 pub fn write_from_user(
    16   canary_.Assert();                                                      =     22     self.canary.assert();
    18   Guard<CriticalMutex> guard{get_lock()};                                =     24     ksync::lock!(let guard = self.lock());
    19   if (!peer()) {                                                         =     25     let Some(peer) = self.peer(&guard) else {
    20     return ZX_ERR_PEER_CLOSED;                                           !     26         return Err(Status::BAD_STATE);
        ^ ! error code differs: C++ returns PEER_CLOSED, Rust returns BAD_STATE
    22   return peer()->WriteSelfLocked(elem_size, ptr, count, actual);         ~     28     peer.write_self_locked(&guard, elem_size, ptr, count)
        ^ ~ only C++ calls peer

  errors   DIFFERENT
           C++:  PEER_CLOSED
           Rust: BAD_STATE
  locks    same
           C++:  lock
           Rust: lock
  flow     if 1, return 2
  comments 2 C++ comments: 2 identical, 0 reworded, 0 missing in Rust
  findings
    ! fifo_dispatcher.cc:20 | fifo_dispatcher.rs:26  error code differs: C++ returns PEER_CLOSED, Rust returns BAD_STATE
    ~ fifo_dispatcher.cc:22 | fifo_dispatcher.rs:28  only C++ calls peer
```

Each pair has a header, the aligned source, a summary, and a findings list
with `file:line` references on both sides. The marker column is:

| Marker | Meaning |
| --- | --- |
| `=` | Aligned and equivalent. |
| `~` | Aligned, with a note: a difference that is often just translation (reworded comment, different helper calls). |
| `!` | Aligned, with an issue: different error code, lock, propagation, or success/failure. |
| `<` | Only in C++. |
| `>` | Only in Rust. |

Findings start with `!` (issue) or `~` (note), so `grep '^    !'` lists the
issues. At the end the report lists functions it could not pair and the FFI
shims it recognized.

`tests/fixtures/fifo/expected.txt` is a complete example; its Rust contains
four planted mistakes (a different error code, a rollback replaced by `?`, a
lock moved ahead of the argument checks, and a dropped comment).

## How it works

1. **Inputs.** The change is turned into the C++ files as they were before and
   the Rust files after, with the lines the change touched. In `git` mode
   files are read at both revisions; in `patch` mode they are rebuilt from the
   hunks (or read by blob id with `-C`).
2. **Units.** Each function is parsed with tree-sitter and flattened into
   units: comments, statements, and the headers of `if`/`else`/loops/`switch`/
   `match`/`case`, plus `return`/`break`/`continue`. Each unit gets
   language-neutral features: normalized calls and identifiers
   (`CommitRange` = `commit_range`, `lock_` = `self.lock`, `kFoo` = `FOO`),
   error codes (`ZX_ERR_NO_MEMORY` = `Status::NO_MEMORY`), locks
   (`Guard<Mutex> guard{&lock_}` = `self.lock.lock()` = `ksync::lock!(...)`),
   whether it propagates an error, and comment words.
   Common idioms are normalized so they line up:
   `if (status != ZX_OK) return status;` reads as `?`,
   `return c ? A : B;` as `if c { A } else { B }`,
   `*out = x; return ZX_OK;` as `Ok(x)`, and `case A: case B:` as `A | B =>`.
3. **Pairing.** C++ functions are paired with Rust functions by, in order:
   explicit `--pair`s; the FFI shim pattern (the new C++ body calls
   `rust_<class>_<method>`, whose `#[no_mangle]` definition forwards to the
   Rust method, or the shim's name spells the C++ function's); then a score
   combining name similarity and body similarity, where comments count
   double. Rust functions still unpaired are looked up in the repository
   (files named like the Rust file, then `git grep` for the CamelCase and
   snake_case names), so Rust that ports C++ the change did not delete is
   still compared. Doc comments on C++ declarations in headers are attached
   to the definitions, since that is where Rust doc comments come from.
4. **Alignment.** Units are aligned with an order-preserving weighted LCS over
   unit similarity. Aligned units are compared feature by feature; unaligned
   ones are reported, and a unit that resembles one on the other side at a
   different position is reported as possibly reordered.

## Library

The crate is a library with a thin CLI on top:

```rust
use babeldiff::{analyze, input::ChangeSet, render};

let cs = ChangeSet::from_files(&[(cpp_path, cpp_text), (rust_path, rust_text)]);
let report = babeldiff::run(&cs, &analyze::Options::default(), &mut analyze::NoFinder);
for pair in &report.pairs {
    for finding in &pair.findings {
        println!("{:?} {}", finding.severity, finding.message);
    }
}
print!("{}", render::render(&report, &render::RenderOptions::default()));
```

`babeldiff::git::Git` and `babeldiff::git::RepoFinder` provide the git
integration; implement `analyze::CppFinder` to look up C++ some other way.

## Limitations

- A C++ function split across several Rust functions (or the reverse) is
  paired with its best match only; the rest shows as unpaired.
- Macros are compared by name and the calls inside their arguments, not
  expanded.
- Similarity thresholds are tuned on a handful of Zircon changes; use
  `--pair` when the matcher gets a pairing wrong.

## Development

```sh
cargo test          # unit tests and golden tests over tests/fixtures
BLESS=1 cargo test  # rewrite expected outputs after an intentional change
```
