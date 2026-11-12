# opurom-runtime-adapters

Backend runtime adapters for Opurom capability, and the local conformance harness that
proves a narrow, checkable claim: **one business logic, multiple backend
runtimes**.

The same CRUD logic, derived from the `models` / `api` arrays of a full-stack
IR, answers the same request set with **byte-identical** JSON on four
different local runtimes.

This project is a conformance harness. It is **not** a deployment tool: there
is no cloud provider, no serverless bundle, no database and no service
packaging anywhere in this repository.

## Adapter matrix

| Runtime | Status | What it is | IO | Conformance |
| --- | --- | --- | --- | --- |
| `native` | **Verified** | In-process Rust reference. The business logic compiled into the binary. | none | byte-identical to native (it is the reference) |
| `node` | **Verified** | Real `node` subprocess running `adapters/node-host.mjs`, line-delimited JSON over stdin/stdout. | subprocess pipes | byte-identical to native |
| `edge` | **Verified** | Pure request -> response function, zero IO, session state in the adapter. | none | byte-identical to native |
| `embedded` | **Verified** | Single-threaded, fixed-capacity storage, no heap growth beyond a documented budget. | none | byte-identical to native |
| `serverless-cloud` | **Planned** | Nothing. | — | 0 checks, **no deployment attempted** |

`Verified` means exactly this: the local conformance checks passed on the
machine that ran them. It never means "deployed", "benchmarked" or "production
ready".

### Capabilities

Each adapter reports what its runtime can do (`Capabilities`):

| Runtime | async_io | threads | fs | network | max_body |
| --- | --- | --- | --- | --- | --- |
| `native` | false | false | false | false | 1 MiB |
| `node` | true | true | false | false | 1 MiB |
| `edge` | false | false | false | false | 1 MiB |
| `embedded` | false | false | false | false | 1 MiB |

`max_body` is the 1 MiB request body cap from
`opurom-examples/the framework implementation` framework rules. `node` reports `async_io` and
`threads` because it drives a real subprocess with a reader thread; it performs
no filesystem or network IO of its own.

### The embedded budget

`embedded` keeps every byte of persistent state inline:

| Limit | Value |
| --- | --- |
| models (tables) | 8 |
| rows per table | 32 |
| fields per row | 8 |
| bytes per model name | 24 |
| bytes per row key | 32 |
| bytes per field name | 24 |
| bytes per string value | 64 |

A write that would exceed a limit is refused with
`AdapterError::BudgetExceeded` instead of growing the heap. The resident
footprint is constant and does not change with the row count
(`Embedded::resident_bytes()`).

Transient allocations while building a response are proportional to the
response size, as in any JSON runtime. The *persistent* footprint is the fixed
budget above.

## The ABI

```rust
pub trait RuntimeAdapter {
    fn name(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn invoke(&mut self, req: &Request) -> Result<Response, AdapterError>;
    fn shutdown(&mut self) -> Result<(), AdapterError>;
}
```

`Request` carries `method`, `path` and a JSON `body`. `Response` carries an
HTTP-style `status` and a JSON `body`. `AdapterError` is a value: a missing
`node` binary is `AdapterError::HostUnavailable`, a silent host is
`AdapterError::Timeout`. Neither is a panic.

This is a *backend runtime* ABI. It is distinct from the 0.1.0 `PlatformAdapter`
ABI in `opurom-examples/the framework implementation`, which describes UI hosts (windowing,
input, presentation).

## The conformance report

```sh
cargo run -- examples/todo-fullstack.opuir.json
```

```text
Opurom runtime conformance report
app: TodoApp
models: 1  api routes: 2

name | status | checks | result
--- | ------ | ------ | ------
native | Verified | 7 | pass (reference runtime)
node | Verified | 7 | pass (byte-identical to native)
edge | Verified | 7 | pass (byte-identical to native; local function only, not deployed)
embedded | Verified | 7 | pass (byte-identical to native; fixed capacity, no heap growth)
serverless-cloud | Planned | 0 | no deployment attempted

Verified means: local conformance checks passed on this machine only.
Not verified: real serverless/edge cloud deployment, Node.js service bundling,
PostgreSQL/MySQL drivers, multi-tenant isolation, latency/fps benchmarks.
```

The `serverless-cloud` row is always present and always `Planned`, so nobody
can mistake local verification for a cloud deployment.

A plain 0.1.0 IR (no `models` / `api` arrays) still works: the model set is
empty and the runtimes answer the single structural "unknown route" check.

```sh
cargo run -- examples/plain.opuir.json
```

## The sample request set

The checks are derived from the IR, not hard-coded:

| Check | Request | Expected |
| --- | --- | --- |
| `list-empty` | `GET /todos` | `200 []` |
| `create` | `POST /todos` | `201` with the created row |
| `list-one` | `GET /todos` | `200` with one row |
| `get` | `GET /todos/1` | `200` with the row |
| `update` | `PUT /todos/1` | `200` with the updated row |
| `delete` | `DELETE /todos/1` | `200 {"deleted":true}` |
| `not-found` | `GET /todos/1` | `404 {"error":"not_found"}` |

The responses follow `opurom-examples/the framework implementation` framework rules.

## How to run the conformance suite

```sh
cargo fmt
cargo test
node --check adapters/node-host.mjs
```

`cargo test` runs:

- `tests/conformance.rs` — the seven sample requests against all four
  runtimes, asserting byte-identical canonical responses; the exact expected
  bytes for the documented CRUD answers; a missing `node` binary yielding a
  clean `AdapterError`; a deliberately non-responsive host timing out in under
  2 seconds; the embedded runtime refusing to exceed its fixed capacity; and
  a plain 0.1.0 IR working with an empty model set.
- `tests/report.rs` — the report format, the honest status column, and clean
  failure on a bad IR.

The Node host script lives at `adapters/node-host.mjs` and is found through
`CARGO_MANIFEST_DIR`, so the tests run from any working directory. The
test-only stub host `tests/fixtures/silent-host.mjs` completes the handshake
and then deliberately never answers.

## Adding a new runtime

1. Implement `RuntimeAdapter` in `src/adapters/<name>.rs`: `name`,
   `capabilities`, `invoke`, `shutdown`. Errors are values, never panics.
2. Apply the same business logic from `src/logic.rs` (or implement the same
   contract in the target language). The conformance suite compares
   canonical response bytes, so the semantics must match exactly.
3. Return canonical JSON: object keys sorted, no insignificant whitespace.
   `adapters::parse_canonical_body` enforces this on the way in.
4. Bound every wait. A runtime that stops answering must produce
   `AdapterError::Timeout` within the configured limit, never a hang.
5. Add a row to the report in `src/main.rs` with an honest `Status`.
   `Verified` requires that the local checks actually ran and passed;
   `Scaffold` means the adapter exists but its checks did not all pass;
   `Planned` means no adapter and no checks.
6. Add the runtime to `tests/conformance.rs` and assert its responses are
   byte-identical to `native`.
7. Add the runtime to the matrix table in this README, with the same honesty
   rule: list what is *not* verified.
8. Keep the `serverless-cloud | Planned | 0 | no deployment attempted` row.

## Honest status

### Verified (local conformance only)

- `native`: in-process Rust, seven checks, byte-identical by construction.
- `node`: a real `node` subprocess with a real handshake, real round trips and
  a bounded timeout. The host implements the same business logic in
  JavaScript.
- `edge`: a pure request -> response function with zero IO.
- `embedded`: single-threaded, fixed capacity, constant resident footprint.

### NOT verified

- **Real serverless or edge cloud deployment.** No cloud provider is contacted,
  nothing is deployed, no bundle is uploaded. The `serverless-cloud` row is
  `Planned` with 0 checks.
- **Node.js service bundling.** `adapters/node-host.mjs` runs from the source
  tree. No `npm` bundle, no bundler, no lockfile, no dependency install.
- **PostgreSQL / MySQL.** No SQL driver exists here. Storage is an in-memory
  map (native, node, edge) or a fixed inline table (embedded).
- **Multi-tenant isolation.** One adapter instance, one session table. There is
  no tenant concept.
- **Latency / fps / cold-start benchmarks.** No numbers are reported. The
  conformance suite measures correctness, not performance.
- **Concurrency and load behaviour.** The embedded runtime is single-threaded
  by construction; the others are exercised one request at a time.
- **Production hardening.** No TLS, no authentication, no rate limiting, no
  request logging, no crash recovery, no observability.

## Layout

```text
src/lib.rs             the RuntimeAdapter ABI, Request/Response/Capabilities/Status
src/ir.rs              parsing of the capability `models` / `api` arrays
src/logic.rs           the one business logic and the derived sample checks
src/store.rs           MapStore (native/edge/node) and FixedStore (embedded)
src/report.rs          the conformance harness helpers
src/main.rs            the `opurom-runtime-adapters` report binary
adapters/node-host.mjs the Node.js runtime host
tests/conformance.rs   the cross-runtime conformance suite
tests/report.rs        the report binary suite
tests/fixtures/        the test-only non-responsive host stub
examples/              a full-stack IR and a plain 0.1.0 IR
```

## License

Follow the repository root policy of the Opurom workspace.
