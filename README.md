# opurom-runtime-adapters

Backend runtime adapters for Opurom, and the local conformance harness that
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
the framework implementation
framework rules. `node` reports `async_io` and `threads` because it drives a real
subprocess with a reader thread; it performs no filesystem or network IO of its
own.

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

`Request` carries `method`, `path`, a JSON `body` and an optional
`content_type` (the request Content-Type header; `None` means the header was
absent). `Response` carries an HTTP-style `status`, a JSON `body` and a
lowercased header map stamped with `content-type: application/json` by every
runtime. `AdapterError` is a value: a missing `node` binary is
`AdapterError::HostUnavailable`, a silent host is
`AdapterError::Timeout`. Neither is a panic.

This is a *backend runtime* ABI. It is distinct from the 0.1.0 `PlatformAdapter`
ABI in
the framework implementation,
which describes UI hosts (windowing, input, presentation).

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

The responses follow
the framework implementation
framework rules.

### Health route

`GET /health` answers `200 {"ok":true,"app":"<name>","version":"0.1.0"}` on all
four runtimes, byte-identically. It is fixed and model-independent, so it is
answered before model route matching and only for `GET`/`HEAD`; any other
method on `/health` answers
`405 {"error":"method_not_allowed"}` with `allow: GET, HEAD` because
`/health` is a registered GET route. The `app` value is the IR's `app` field.

### Method dispatch

Dispatch mirrors the framework router:

| Request | Answer |
| --- | --- |
| unknown path, any method | `404 {"error":"not_found"}` |
| known path, no route for the method (`PATCH /todos`, `POST /todos/1`) | `405 {"error":"method_not_allowed"}` with an `allow` header |
| `HEAD` on a path a `GET` route serves | the `GET` route's JSON response |

The `allow` value lists the path's registered methods, deduped in the fixed
order `GET, HEAD, POST, PUT, DELETE`: the collection route answers
`allow: GET, HEAD, POST`, the item route `allow: GET, HEAD, PUT, DELETE`. A
path no api pattern matches stays `404` whatever the method, without `allow`.
`HEAD` resolves against the `GET` route, so its JSON-level answer is
byte-identical to `GET`; the wire layer omits the body.

### ETag and conditional requests

Every successful (2xx) answer with a JSON body carries an `etag` header: a
deterministic, non-cryptographic FNV-1a validator over the canonical JSON body
bytes, spelled as a quoted 16-digit lowercase hex value. It mirrors the
framework's `etag_for` and is always emitted, while the framework keeps the
feature opt-in behind `OPR_ETAG`.

A `GET` whose `If-None-Match` matches the validator becomes a bodyless `304`
that keeps only validator and caching headers and drops representation headers
such as `content-type`. The comparison is the framework's weak/list form: `*`
matches everything, `W/` prefixes are stripped before comparing, and a
malformed validator simply never matches, so the answer stays a `200`. Only
`GET` is conditional; other methods answer normally whatever they carry. A
`304` gains `Vary: Accept-Encoding` only when the body would actually be
gzip-compressed by the host transport; the adapters themselves answer the
pre-compression representation and never emit `Content-Encoding` — compression
is a host transport feature, outside this ABI.

### CORS

CORS is operator configuration, not part of the IR: each adapter takes a
`LayerOptions` value (defaulting to off, mirroring the framework's
environment-driven defaults) and the node host receives it in the handshake
frame's `options` section.

With an origin configured, the behavior mirrors the framework's CORS layer:

- every `OPTIONS` request, on any path, is a fixed-allow-list preflight:
  a bodyless `204` with `access-control-allow-origin`,
  `access-control-allow-methods: GET, POST, PUT, DELETE, OPTIONS`,
  `access-control-allow-headers: Authorization, Content-Type` and
  `Vary: Origin`;
- every routed answer carries `access-control-allow-origin` and
  `Vary: Origin`, including error answers, and a `304` keeps the
  `Access-Control-*` header because the CORS stage runs before the ETag stage;
- the request's `Origin` header is never used to build a response header —
  the origin is emitted verbatim from the configuration.

Without configuration `OPTIONS` stays an ordinary routed method answering
`405`, pinned by the method-dispatch suite, and no response carries CORS
headers.

### Authentication

Bearer authentication is operator configuration like CORS: `LayerOptions`
carries an `auth_token` (default off, mirroring an unset `OPR_AUTH_TOKEN`) and
the node host receives it in the handshake frame's `options` section.

With a token configured, the behavior mirrors the framework's gate:

- every request must carry `Authorization: Bearer <token>`; a missing or
  mismatched token, or a non-`Bearer` scheme, answers
  `401 {"error":"unauthorized"}`;
- the scheme comparison is case-insensitive, and the token comparison is
  constant time in the lengths (both sides digest to a fixed width and
  XOR-accumulate without an early exit);
- `GET /health` is exempt, and a CORS preflight bypasses the gate;
- the `401` still walks the response stages, so it carries the JSON content
  type and, under CORS, the origin header.

This is a development token, not a production identity system: there are no
users, scopes, expiry or credential storage.

### Error paths and list order

A table-driven case set in `tests/conformance.rs` runs each case against all
four runtimes and asserts byte-identical canonical responses, pinned to exact
expected bytes. It covers not-found ids (missing, malformed, non-canonical and
deleted ids across get, update and delete), invalid payloads (non-object
bodies, missing fields, wrong field and key types), duplicate ids (explicit
conflict, auto-assigned ids, reuse after delete), and empty collections (on a
fresh runtime and after a delete).

List responses are in insertion order on every runtime. The embedded store
compacts its fixed slots on delete, so reusing a freed slot cannot reorder the
remaining rows.

### Content-Type handling

A second table-driven case set in `tests/conformance.rs` pins the media-type
rule to exact bytes on all four runtimes. The rule is identical everywhere and
is applied before route matching:

- Every response answers with the `content-type: application/json` header,
  stamped by each runtime's response builder and pinned in the conformance
  bytes.
- header absent, or `application/json` (case-insensitive, `;` parameters such
  as `; charset=utf-8` ignored): the body is processed as JSON, so a JSON
  payload answers byte-identically with and without the header;
- any other media type: `415 {"error":"unsupported_media_type"}` for every
  method;
- a `null` body — the wire form of a missing or empty payload — keeps the
  `400 {"error":"invalid_body"}` answer for create and update.

Every response also *carries* the response header `content-type:
application/json`; it is part of the canonical response bytes, stamped once by
`Response::new` on each side.

### Request body cap

A third table-driven case set in `tests/conformance.rs` pins the 1 MiB
`max_body` cap to exact bytes on all four runtimes. The cap counts whole
UTF-8 bytes of the canonical request body and is applied before route
matching, so the rule is identical everywhere:

- a body of exactly 1 MiB is still processed (`>` comparison, not `>=`);
- one byte over the cap answers `413 {"error":"payload_too_large"}` for
  every method, before route matching or payload handling;
- a multi-byte body crosses the cap by byte count alone: a body of
  two-byte characters whose UTF-16 code unit count stays under 1 MiB still
  answers 413 once its byte count is over, because the Node host counts
  UTF-8 bytes exactly like the Rust adapters.

## Path parameters

An API route may declare more than one `:name` segment. Each such segment is
bound to its request segment under the declared name. The row key for `get`,
`update` and `delete` is read from the parameter whose name matches the model's
key field; other bound parameters do not supply the row key. Literal segments
must match exactly.

## How to run the conformance suite

```sh
cargo fmt
cargo test
node --check adapters/node-host.mjs
```

`cargo test` runs:

- `tests/conformance.rs` — the seven sample requests against all four
  runtimes, asserting byte-identical canonical responses; the error-path and
  list-order table; the content-type table; the method-dispatch table; the
  body-cap table; the health-route table; a response content-type header test;
  a stale-protocol host refused at the handshake; the exact expected bytes for
  the documented CRUD answers;
  two-parameter routes dispatching from the named key on all four runtimes;
  a missing `node` binary yielding a clean `AdapterError`; a deliberately
  non-responsive host timing out in under 2 seconds; the embedded runtime
  refusing to exceed its fixed capacity; and a plain 0.1.0 IR working with an
  empty model set.
- `tests/report.rs` — the report format, the status column, and clean
  failure on a bad IR.

The Node host script lives at `adapters/node-host.mjs` and is found through
`CARGO_MANIFEST_DIR`, so the tests run from any working directory. The
test-only stub host `tests/fixtures/silent-host.mjs` completes the handshake
and then deliberately never answers.

## Adding a new runtime

1. Implement `RuntimeAdapter` in `src/adapters/<name>.rs`: `name`,
   `capabilities`, `invoke`, `shutdown`. Errors are values, never panics.
2. Apply the same business logic from `src/logic.rs` (or implement the same
   specification in the target language). The conformance suite compares
   canonical response bytes, so the semantics must match exactly.
3. Return canonical JSON: object keys sorted, no insignificant whitespace.
   `adapters::parse_canonical_body` enforces this on the way in.
4. Bound every wait. A runtime that stops answering must produce
   `AdapterError::Timeout` within the configured limit, never a hang.
5. Add a row to the report in `src/main.rs` with a truthful `Status`.
   `Verified` requires that the local checks actually ran and passed;
   `Scaffold` means the adapter exists but its checks did not all pass;
   `Planned` means no adapter and no checks.
6. Add the runtime to `tests/conformance.rs` and assert its responses are
   byte-identical to `native`.
7. Add the runtime to the matrix table in this README, with the same rule:
   state what the local checks do and do not cover.
8. Keep the `serverless-cloud | Planned | 0 | no deployment attempted` row.

## Verified status

Verified by the local conformance suite in this crate:

- `native`: in-process Rust, seven checks, byte-identical by construction.
- `node`: a real `node` subprocess with a real handshake, real round trips and
  a bounded timeout. The host implements the same business logic in
  JavaScript.
- `edge`: a pure request -> response function with zero IO.
- `embedded`: single-threaded, fixed capacity, constant resident footprint.

## Scope of this repository

- **Local conformance only.** No cloud provider is contacted and nothing is
  deployed, so the `serverless-cloud` row is `Planned` with 0 checks.
- **Source-tree host.** `adapters/node-host.mjs` runs from the source tree:
  no `npm` bundle, no bundler, no lockfile, no dependency install.
- **In-process storage.** No SQL driver exists here. Storage is an in-memory
  map (native, node, edge) or a fixed inline table (embedded).
- **One session table.** One adapter instance, one session table, no tenant
  concept.
- **Correctness measurements.** The conformance suite measures correctness, not
  performance; no latency, fps or cold-start numbers are reported.
- **One request at a time.** The embedded runtime is single-threaded by
  construction; the others are exercised one request at a time.
- **Plain local serving.** No TLS, authentication, rate limiting, request
  logging, crash recovery or observability.

## Layout

```text
src/lib.rs             the RuntimeAdapter ABI, Request/Response/Capabilities/Status
src/ir.rs              parsing of the `models` / `api` arrays
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
