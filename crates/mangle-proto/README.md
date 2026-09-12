# mangle-proto

Protobuf / [Connect RPC](https://connectrpc.com) interface for
[Mangle](https://codeberg.org/TauCeti/mangle-rs): the canonical,
schema-less encoding of Mangle facts and the `MangleService` RPC interface
used by `mangle-server`.

The design is documented in
[`../mangle-server/RPC_DESIGN.md`](../mangle-server/RPC_DESIGN.md). In
short:

- **Canonical encoding** (`mangle/value.proto`): `Value` mirrors Mangle's
  value domain (keeping the name/string distinction, time/duration as
  nanosecond integers, …); `Fact` is the row encoding; `FactBatch` is the
  columnar bulk encoding with typed packed columns.
- **Service** (`mangle/service.proto`): streaming `Query`/`Eval` and
  client-streaming `InsertFacts`/`RetractFacts`, plus unary program
  management.
- **Typed overlays** (planned): generated per-program `.proto` files where
  each predicate is a message; they transcode to/from the canonical
  encoding client-side, keeping the server generic.

Rust types are generated with [buffa](https://crates.io/crates/buffa)
(the protobuf runtime connect-rust is built on) and
[connectrpc-build](https://crates.io/crates/connectrpc-build); `protoc` is
provided by `protoc-bin-vendored`, so the build is hermetic.

The crate also provides conversions between the wire types and
`mangle_common::Value` / `Vec<Value>` tuples, in both row and columnar
form, for owned and zero-copy view inputs.
