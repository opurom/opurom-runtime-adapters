// Opurom Node.js runtime host.
//
// Implements the same CRUD business logic as the Rust adapters, driven by the
// IR `models`/`api` arrays handed over during the handshake, and speaks
// line-delimited JSON over stdin/stdout:
//
//   -> {"type":"handshake","ir":{...},"options":{"corsOrigin":null,"authToken":null}}
//   <- {"type":"ready","ok":true,"protocol":"opurom-runtime-adapters/4"}
//   -> {"type":"invoke","request":{"method":"GET","path":"/todos","body":null,"contentType":null,"headers":{}}}
//   <- {"type":"response","status":200,"body":"[...]","headers":{"content-type":"application/json"}}
//   -> {"type":"shutdown"}
//   <- {"type":"bye"}
//
// The `body` field is a canonical JSON *string*: object keys sorted, no
// insignificant whitespace. That is the byte form the Rust side compares.

import { createHash } from 'node:crypto';
import readline from 'node:readline';

const MAX_BODY_BYTES = 1024 * 1024;
// Frame schema v4: response frames carry a required `headers` map since /2,
// request frames since /3, the handshake carries layer options since /4, and
// capability calls ride `capability` frames since /5. The protocol id is
// asserted by the Rust side at the handshake, so a stale copy of this script
// fails fast instead of silently dropping frame fields.
const PROTOCOL = 'opurom-runtime-adapters/5';

// Responses larger than this many bytes would be gzip-compressed by the host
// transport, which a `304` must record with `Vary: Accept-Encoding`.
const GZIP_THRESHOLD_BYTES = 1024;

// Deterministic, non-cryptographic FNV-1a validator over the UTF-8 bytes of
// the canonical JSON body, mirroring `etag_for` on the Rust side. BigInt keeps
// the 64-bit arithmetic exact; no third-party dependency is needed.
function etagFor(body) {
  let hash = 0xcbf29ce484222325n;
  for (const byte of Buffer.from(body, 'utf8')) {
    hash ^= BigInt(byte);
    hash = (hash * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return '"' + hash.toString(16).padStart(16, '0') + '"';
}

// Weak/list comparison for `If-None-Match`, mirroring the framework: `*`
// matches everything, weak prefixes are stripped before comparing, and a
// malformed validator simply never matches (staying a `200`).
function ifNoneMatchMatches(raw, etag) {
  return raw.split(',').some((entry) => {
    const candidate = entry.trim();
    const strong = candidate.startsWith('W/')
      ? candidate.slice(2)
      : candidate.startsWith('w/')
        ? candidate.slice(2)
        : candidate;
    return candidate === '*' || strong.trim() === etag;
  });
}

// True when the request offers gzip with a nonzero quality, mirroring the
// framework's `acceptsGzip` over the lowercase header map.
function acceptsGzip(headers) {
  const raw = headers['accept-encoding'];
  if (typeof raw !== 'string') return false;
  return raw.split(',').some((entry) => {
    const fields = entry.trim().split(';');
    const coding = (fields[0] || '').trim();
    if (coding.toLowerCase() !== 'gzip') return false;
    return !fields.slice(1).some((parameter) => {
      const index = parameter.indexOf('=');
      if (index < 0) return false;
      const name = parameter.slice(0, index).trim();
      if (name.toLowerCase() !== 'q') return false;
      return Number.parseFloat(parameter.slice(index + 1).trim()) === 0;
    });
  });
}

let ir = { app: '', models: [], api: [] };
// Operator layer options handed over in the handshake frame; every field
// defaults to off, mirroring the framework's environment defaults.
let layerOptions = { corsOrigin: null, authToken: null };
// Build identity handed over at the handshake, mirroring the Rust
// compile-time constant.
let appVersion = '';
/** model name -> Map(key -> row) */
const tables = new Map();
/** Capability storage, host-local and separate from the CRUD tables. */
const capStore = new Map();

function canonicalJson(value) {
  if (value === null || value === undefined) return 'null';
  if (Array.isArray(value)) {
    return '[' + value.map(canonicalJson).join(',') + ']';
  }
  if (typeof value === 'object') {
    const keys = Object.keys(value).sort(compareBytes);
    const parts = keys.map(
      (key) => JSON.stringify(key) + ':' + canonicalJson(value[key])
    );
    return '{' + parts.join(',') + '}';
  }
  return JSON.stringify(value);
}

// Byte-wise key order, matching the Rust serializer's key order.
function compareBytes(a, b) {
  return Buffer.compare(Buffer.from(a, 'utf8'), Buffer.from(b, 'utf8'));
}

function segments(path) {
  return path.split('/').filter((part) => part.length > 0);
}

function modelByName(name) {
  return ir.models.find((model) => model.name === name) || null;
}

function keyInfo(model) {
  const primary = model.fields.find((field) => field.primary);
  if (primary) {
    return { name: primary.name, kind: primary.type, implicit: false };
  }
  // the framework implementation: zero primary fields is legal, an auto rowid is used.
  return { name: 'id', kind: 'int', implicit: true };
}

function matchesKind(kind, value) {
  if (kind === 'int') return typeof value === 'number' && Number.isSafeInteger(value);
  if (kind === 'string') return typeof value === 'string';
  if (kind === 'bool') return typeof value === 'boolean';
  return false;
}

function typedKey(kind, keyValue) {
  return kind === 'int' ? Number(keyValue) : keyValue;
}

// Canonical key rule: an int key is the decimal spelling of its value, so
// leading zeros are not keys. A string key is used verbatim.
function parseKey(kind, raw) {
  if (kind !== 'int') return raw;
  if (!/^-?\d+$/.test(raw)) return null;
  const value = Number(raw);
  if (!Number.isSafeInteger(value)) return null;
  return String(value) === raw ? raw : null;
}

function table(model) {
  let rows = tables.get(model);
  if (!rows) {
    rows = new Map();
    tables.set(model, rows);
  }
  return rows;
}

// The parameters bound by one api pattern, or null when the pattern does not
// match. Parameters bind by name, not by position, so the key field's
// parameter wins among several.
function matchPattern(pattern, requestSegments) {
  const parts = segments(pattern);
  if (parts.length !== requestSegments.length) return null;
  const params = new Map();
  for (let i = 0; i < parts.length; i += 1) {
    if (parts[i].startsWith(':')) {
      params.set(parts[i].slice(1), requestSegments[i]);
    } else if (parts[i] !== requestSegments[i]) {
      return null;
    }
  }
  return params;
}

// The handler verb a request method resolves to, mirroring the framework
// router: a HEAD request is served by the matching GET route (this
// JSON-level ABI returns the handler's response; the wire layer omits the
// body), and a method no route registers resolves to null.
function verbFor(method, hasParams) {
  if (method === 'GET' || method === 'HEAD') return hasParams ? 'get' : 'list';
  if (method === 'POST') return 'create';
  if (method === 'PUT') return 'update';
  if (method === 'DELETE') return 'delete';
  return null;
}

// The HTTP method a declared handler verb registers, mirroring the framework
// router's route table. Unknown handler verbs register nothing.
function handlerMethod(handler) {
  if (handler === 'list' || handler === 'get') return 'GET';
  if (handler === 'create') return 'POST';
  if (handler === 'update') return 'PUT';
  if (handler === 'delete') return 'DELETE';
  return null;
}

// The `Allow` value for a matched path: every method the matching patterns
// register, deduped, in the fixed order the framework router emits.
function allowValue(methods) {
  return ['GET', 'HEAD', 'POST', 'PUT', 'DELETE']
    .filter((method) => methods.has(method))
    .join(', ');
}

// Outcome of matching a request against the IR `api` array, mirroring the
// framework router: every matching pattern is considered, a pattern that
// matches but declares no handler for the resolved verb answers 405 with an
// `Allow` list of the path's registered methods, and only a path no pattern
// matches is 404. An unmatched outcome carries only `pathMatched` plus the
// computed `allow` value.
function resolveRoute(requestSegments, method) {
  let pathMatched = false;
  const registered = new Set();
  for (const api of ir.api) {
    const params = matchPattern(api.path, requestSegments);
    if (params === null) continue;
    pathMatched = true;
    for (const handler of api.handlers) {
      const http = handlerMethod(handler);
      if (http === null) continue;
      registered.add(http);
      // HEAD is served by the GET route, so it appears alongside it.
      if (http === 'GET') registered.add('HEAD');
    }
    const verb = verbFor(method, params.size > 0);
    if (verb !== null && api.handlers.includes(verb)) return { api, params, verb };
  }
  return { pathMatched, allow: allowValue(registered) };
}

function nextId(model, keyName) {
  let max = 0;
  for (const row of table(model).values()) {
    const value = row[keyName];
    if (typeof value === 'number' && value > max) max = value;
  }
  return max + 1;
}

function buildRow(model, key, keyValue, body, existing) {
  const row = {};
  for (const field of model.fields) {
    if (field.name === key.name) {
      row[field.name] = typedKey(key.kind, keyValue);
      continue;
    }
    if (Object.prototype.hasOwnProperty.call(body, field.name)) {
      const value = body[field.name];
      if (!matchesKind(field.type, value)) {
        return {
          error: { status: 400, body: { error: 'invalid_field', field: field.name } },
        };
      }
      row[field.name] = value;
    } else if (existing && Object.prototype.hasOwnProperty.call(existing, field.name)) {
      row[field.name] = existing[field.name];
    } else if (field.default !== null && field.default !== undefined) {
      row[field.name] = field.default;
    } else if (!existing) {
      return {
        error: { status: 400, body: { error: 'invalid_body', field: field.name } },
      };
    }
  }
  if (key.implicit) row[key.name] = typedKey(key.kind, keyValue);
  return { row };
}

function notFound() {
  return { status: 404, body: { error: 'not_found' } };
}

function methodNotAllowed(allow) {
  return { status: 405, body: { error: 'method_not_allowed' }, headers: { allow } };
}

// The media-type rule shared by every runtime: the base type before any `;`
// parameters, compared case-insensitively. An absent header is JSON by
// default; anything else is refused with 415 before route matching.
function jsonMedia(contentType) {
  const base = contentType.split(';')[0].trim().toLowerCase();
  return base === 'application/json';
}

function invalidBody(field) {
  const body = { error: 'invalid_body' };
  if (field) body.field = field;
  return { status: 400, body };
}

// The response builder: every JSON response carries the normative
// `content-type`, mirroring `Response::new` on the Rust side; headers a verb
// already attached (the `Allow` list on a 405) are kept. The ETag stage runs
// after the stamp, mirroring the framework's stage order.
function handle(request) {
  // The preflight answer bypasses routing and auth entirely (mirroring the
  // framework), authentication gates everything else before routing, and every
  // answer then walks the response stages in the framework's fixed order: CORS
  // before ETag.
  const origin = layerOptions.corsOrigin;
  const method = String(request.method || '');
  const headers = requestHeaders(request);
  let response;
  if (origin !== null && method === 'OPTIONS') {
    response = {
      status: 204,
      body: null,
      headers: {
        'access-control-allow-origin': origin,
        'access-control-allow-methods': 'GET, POST, PUT, DELETE, OPTIONS',
        'access-control-allow-headers': 'Authorization, Content-Type',
        vary: 'Origin',
      },
    };
  } else if (!authExempt(request) && !authAuthorized(headers)) {
    response = { status: 401, body: { error: 'unauthorized' } };
    response.headers = { ...response.headers, 'content-type': 'application/json' };
  } else {
    response = serve(request);
    response.headers = { ...response.headers, 'content-type': 'application/json' };
  }
  stageCors(response);
  return stageEtag(response, headers, method);
}

// True when the request may skip the token check: authentication is disabled,
// or the request is `GET /health`, mirroring the framework.
function authExempt(request) {
  if (layerOptions.authToken === null) return true;
  return String(request.method || '') === 'GET' && String(request.path || '') === '/health';
}

// True when the request carries a matching `Authorization: Bearer` value,
// compared in time that depends only on the lengths (mirroring the framework's
// digest-then-XOR gate).
function authAuthorized(headers) {
  const token = layerOptions.authToken;
  if (token === null) return true;
  const raw = headers.authorization;
  if (typeof raw !== 'string') return false;
  const space = raw.indexOf(' ');
  if (space < 0) return false;
  const scheme = raw.slice(0, space);
  const candidate = raw.slice(space + 1);
  if (scheme.toLowerCase() !== 'bearer') return false;
  return constantTimeEq(candidate, token);
}

// Compare two byte strings in time that depends only on their lengths,
// mirroring the framework's `constant_time_eq`: both sides digest to a fixed
// width with SHA-256 and XOR-accumulate without an early exit, and the length
// mismatch is folded into the verdict so unequal-length tokens stay rejected.
function constantTimeEq(expected, provided) {
  const left = createHash('sha256').update(Buffer.from(expected, 'utf8')).digest();
  const right = createHash('sha256').update(Buffer.from(provided, 'utf8')).digest();
  let difference = Buffer.byteLength(expected, 'utf8') ^ Buffer.byteLength(provided, 'utf8');
  for (let i = 0; i < left.length; i += 1) difference |= left[i] ^ right[i];
  return difference === 0;
}

// Stage 2 of the response pipeline, mirroring the framework: a configured
// origin is stamped onto every answer that does not carry one, and the
// negotiation is recorded with `Vary: Origin`.
function stageCors(response) {
  const origin = layerOptions.corsOrigin;
  if (origin === null) return;
  if (!Object.hasOwn(response.headers, 'access-control-allow-origin')) {
    response.headers['access-control-allow-origin'] = origin;
  }
  addVary(response.headers, 'Origin');
}

// Merge one value into `Vary`, mirroring the framework's `addVary`: an
// existing header is appended to as a comma list without duplicates.
function addVary(headers, value) {
  const current = headers.vary;
  if (typeof current === 'string') {
    if (!current.split(',').some((field) => field.trim() === value)) {
      headers.vary = current + ', ' + value;
    }
  } else {
    headers.vary = value;
  }
}

// The request headers other than the Content-Type, keyed by lowercase name.
// An absent or malformed map is simply empty, never a failure.
function requestHeaders(request) {
  const raw = request.headers;
  if (raw === null || raw === undefined || typeof raw !== 'object' || Array.isArray(raw)) {
    return {};
  }
  const headers = {};
  for (const [name, value] of Object.entries(raw)) {
    if (typeof value === 'string') headers[name.toLowerCase()] = value;
  }
  return headers;
}

// The ETag stage, mirroring the framework: every successful non-null answer
// carries the body-hash validator, and a matching `If-None-Match` on GET
// becomes a bodyless `304` that keeps only validator and caching headers.
// Compression itself stays at the host transport layer, so answers are the
// pre-compression representation and never carry `Content-Encoding`.
function stageEtag(response, headers, method) {
  if (response.status < 200 || response.status >= 300 || response.body === null) {
    return response;
  }
  const body = canonicalJson(response.body);
  const etag = etagFor(body);
  response.headers = { ...response.headers, etag };
  const compresses =
    acceptsGzip(headers) && Buffer.byteLength(body, 'utf8') > GZIP_THRESHOLD_BYTES;
  if (
    method === 'GET' &&
    typeof headers['if-none-match'] === 'string' &&
    ifNoneMatchMatches(headers['if-none-match'], etag)
  ) {
    // A `304` keeps validator and caching headers and drops representation
    // headers such as `content-type`, mirroring the framework's retain set.
    const kept = {};
    for (const [name, value] of Object.entries(response.headers)) {
      if (
        name === 'etag' ||
        name === 'vary' ||
        name === 'cache-control' ||
        name.startsWith('access-control-')
      ) {
        kept[name] = value;
      }
    }
    if (compresses) addVary(kept, 'Accept-Encoding');
    return { status: 304, body: '', headers: kept };
  }
  return response;
}

function serve(request) {
  const method = String(request.method || '');
  const path = String(request.path || '');
  const requestBody = request.body === undefined ? null : request.body;

  // The cap is the UTF-8 byte count the Rust adapters compare against
  // `max_body`: string length counts UTF-16 code units, so a multi-byte body
  // would cross the shared cap at a different size on this runtime alone.
  if (Buffer.byteLength(canonicalJson(requestBody), 'utf8') > MAX_BODY_BYTES) {
    return { status: 413, body: { error: 'payload_too_large' } };
  }

  const contentType = typeof request.contentType === 'string' ? request.contentType : null;
  if (contentType !== null && !jsonMedia(contentType)) {
    return { status: 415, body: { error: 'unsupported_media_type' } };
  }

  // framework rules: `/health` is a registered GET route answering the app
  // identity, so it is answered before model route matching and a method it
  // does not serve is a 405 like any other known pattern.
  if (path === '/health') {
    if (method === 'GET' || method === 'HEAD') {
      return { status: 200, body: { ok: true, app: ir.app, version: '0.1.0' } };
    }
    return methodNotAllowed('GET, HEAD');
  }

  const resolved = resolveRoute(segments(path), method);
  if (resolved.api === undefined) {
    return resolved.pathMatched ? methodNotAllowed(resolved.allow) : notFound();
  }
  const { api, params, verb } = resolved;

  const model = modelByName(api.model);
  if (!model) return notFound();

  const key = keyInfo(model);
  let itemKey = null;
  const rawKey = params.get(key.name);
  if (rawKey !== undefined) {
    itemKey = parseKey(key.kind, rawKey);
    if (itemKey === null) return notFound();
  }
  if ((verb === 'get' || verb === 'update' || verb === 'delete') && itemKey === null) {
    return notFound();
  }

  const rows = table(model.name);

  if (verb === 'list') {
    return { status: 200, body: Array.from(rows.values()) };
  }

  if (verb === 'create') {
    if (requestBody === null || typeof requestBody !== 'object' || Array.isArray(requestBody)) {
      return invalidBody(null);
    }
    let keyValue;
    if (key.kind === 'int') {
      if (Object.prototype.hasOwnProperty.call(requestBody, key.name)) {
        const value = requestBody[key.name];
        if (!matchesKind('int', value)) {
          return { status: 400, body: { error: 'invalid_field', field: key.name } };
        }
        keyValue = String(value);
      } else {
        keyValue = String(nextId(model.name, key.name));
      }
    } else {
      if (!Object.prototype.hasOwnProperty.call(requestBody, key.name)) {
        return invalidBody(key.name);
      }
      const value = requestBody[key.name];
      if (typeof value !== 'string') {
        return { status: 400, body: { error: 'invalid_field', field: key.name } };
      }
      keyValue = value;
    }
    if (rows.has(keyValue)) {
      return { status: 409, body: { error: 'conflict' } };
    }
    const built = buildRow(model, key, keyValue, requestBody, null);
    if (built.error) return built.error;
    rows.set(keyValue, built.row);
    return { status: 201, body: built.row };
  }

  if (verb === 'get') {
    const row = rows.get(itemKey);
    if (!row) return notFound();
    return { status: 200, body: row };
  }

  if (verb === 'update') {
    if (requestBody === null || typeof requestBody !== 'object' || Array.isArray(requestBody)) {
      return invalidBody(null);
    }
    const existing = rows.get(itemKey);
    if (!existing) return notFound();
    const built = buildRow(model, key, itemKey, requestBody, existing);
    if (built.error) return built.error;
    rows.set(itemKey, built.row);
    return { status: 200, body: built.row };
  }

  if (!rows.delete(itemKey)) return notFound();
  return { status: 200, body: { deleted: true } };
}

// Capability calls mirror `src/capabilities.rs`: the same method sets, the
// same degradation tiers and the same typed error codes. Storage is
// host-local and separate from the CRUD tables.
function capabilityError(code, message) {
  return { error: { code, message, details: null } };
}

function argumentsObject(args) {
  return args !== null && typeof args === 'object' && !Array.isArray(args) ? args : null;
}

function capabilityStorage(method, args) {
  if (!['set', 'get', 'remove', 'clear', 'keys', 'getAll'].includes(method)) {
    return capabilityError('unimplemented', `method ${method} is not part of storage`);
  }
  const object = argumentsObject(args);
  if (method === 'keys') return { value: Array.from(capStore.keys()) };
  if (method === 'getAll') {
    const all = {};
    for (const [key, value] of capStore) all[key] = value;
    return { value: all };
  }
  if (method === 'clear') {
    capStore.clear();
    return { value: true };
  }
  const key = object === null ? undefined : object.key;
  if (typeof key !== 'string') {
    return capabilityError('invalid_argument', 'storage call needs a string key');
  }
  if (method === 'set') {
    if (object === null || !Object.hasOwn(object, 'value')) {
      return capabilityError('invalid_argument', 'missing argument value');
    }
    const value = object.value === undefined ? null : object.value;
    capStore.set(key, value);
    return { value: true };
  }
  if (method === 'get') return { value: capStore.has(key) ? capStore.get(key) : null };
  if (method === 'remove') return { value: capStore.delete(key) };
  return capabilityError('unimplemented', `method ${method} is not part of storage`);
}

function capabilityAppInfo(method) {
  if (method === 'buildName') return { value: appVersion };
  if (method === 'buildNumber') return { value: null };
  if (method === 'get') {
    return { value: { name: ir.app, buildName: appVersion, buildNumber: null } };
  }
  return capabilityError('unimplemented', `method ${method} is not part of app_info`);
}

// Optional capabilities: null for a read, false for a query, no-op success
// for a command. An unrecognized method is never a fake success.
const NO_OP_METHODS = {
  clipboard: ['getData', 'hasStrings', 'setData'],
  haptics: [
    'vibrate',
    'lightImpact',
    'mediumImpact',
    'heavyImpact',
    'selectionClick',
    'successNotification',
    'warningNotification',
    'errorNotification',
  ],
  system_sound: ['play'],
  system_ui: ['setPreferredOrientations', 'setEnabledSystemUIMode'],
};

function capabilityNoOp(capability, method, args) {
  if (!NO_OP_METHODS[capability].includes(method)) {
    return capabilityError('unimplemented', `method ${method} is not part of ${capability}`);
  }
  if (capability === 'clipboard' && method === 'setData') {
    const object = argumentsObject(args);
    if (object === null || typeof object.text !== 'string') {
      return capabilityError('invalid_argument', 'clipboard call needs a string text');
    }
  }
  if (capability === 'clipboard' && method === 'hasStrings') return { value: false };
  return { value: null };
}

function capability(call) {
  const name = String(call.capability || '');
  const method = String(call.method || '');
  const args = call.arguments === undefined ? null : call.arguments;
  if (name === 'storage') return capabilityStorage(method, args);
  if (name === 'app_info') return capabilityAppInfo(method);
  if (Object.hasOwn(NO_OP_METHODS, name)) return capabilityNoOp(name, method, args);
  return capabilityError(
    'unimplemented',
    `capability ${name} is not implemented by this runtime`
  );
}

function write(message) {
  process.stdout.write(JSON.stringify(message) + '\n');
}

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

rl.on('line', (line) => {
  const trimmed = line.trim();
  if (trimmed.length === 0) return;
  let message;
  try {
    message = JSON.parse(trimmed);
  } catch (error) {
    write({ type: 'error', message: `host received a non-JSON line: ${error.message}` });
    return;
  }
  try {
    if (message.type === 'handshake') {
      ir = {
        app: (message.ir && message.ir.app) || '',
        models: (message.ir && message.ir.models) || [],
        api: (message.ir && message.ir.api) || [],
      };
      const raw = message.options && typeof message.options === 'object' ? message.options : {};
      layerOptions = {
        corsOrigin: typeof raw.corsOrigin === 'string' ? raw.corsOrigin : null,
        authToken: typeof raw.authToken === 'string' ? raw.authToken : null,
      };
      tables.clear();
      capStore.clear();
      appVersion = typeof message.appVersion === 'string' ? message.appVersion : '';
      write({ type: 'ready', ok: true, protocol: PROTOCOL });
      return;
    }
    if (message.type === 'capability') {
      const outcome = capability(message.call || {});
      if (outcome.error) {
        write({ type: 'capability_error', ...outcome.error });
      } else {
        write({ type: 'capability_result', value: outcome.value });
      }
      return;
    }
    if (message.type === 'invoke') {
      const response = handle(message.request || {});
      write({
        type: 'response',
        status: response.status,
        body: canonicalJson(response.body),
        headers: response.headers,
      });
      return;
    }
    if (message.type === 'shutdown') {
      write({ type: 'bye', ok: true });
      rl.close();
      process.exit(0);
    }
    write({ type: 'error', message: `unknown host message type ${message.type}` });
  } catch (error) {
    write({ type: 'error', message: `host failure: ${error.message}` });
  }
});
