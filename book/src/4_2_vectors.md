# Vectors

A vector is a native data structure of Rune which is a dynamic list of values.
A vector isn't typed, and can store *any* rune values.

```rust,noplaypen
{{#include ../../scripts/book/4_2/vectors.rn}}
```

```text
$> cargo run -- scripts/book/4_2/vectors.rn
0 = Integer(42)
0 = StaticString("Hello")
== Unit (7.5299ms)
```