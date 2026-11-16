// Opurom Node.js runtime host.
//
// Implements the same CRUD business logic as the Rust adapters, driven by the
// IR `models`/`api` arrays handed over during the handshake, and speaks
// line-delimited JSON over stdin/stdout:
//
//   -> {"type":"handshake","ir":{...}}
//   <- {"type":"ready","ok":true,"protocol":"opurom-runtime-adapters/2"}
//   -> {"type":"invoke","request":{"method":"GET","path":"/todos","body":null,"contentType":null}}
//   <- {"type":"response","status":200,"body":"[...]","headers":{"content-type":"application/json"}}
//   -> {"type":"shutdown"}
//   <- {"type":"bye"}
//
// The `body` field is a canonical JSON *string*: object keys sorted, no
// insignificant whitespace. That is the byte form the Rust side compares.

import readline from 'node:readline';

const MAX_BODY_BYTES = 1024 * 1024;
// Frame schema v2: response frames carry a required `headers` map. The
// protocol id is asserted by the Rust side at the handshake, so a stale copy
// of this script fails fast instead of silently dropping frame fields.
const PROTOCOL = 'opurom-runtime-adapters/2';

let ir = { app: '', models: [], api: [] };
/** model name -> Map(key -> row) */
const tables = new Map();

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
// already attached (the `Allow` list on a 405) are kept.
function handle(request) {
  const response = serve(request);
  response.headers = { ...response.headers, 'content-type': 'application/json' };
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
      tables.clear();
      write({ type: 'ready', ok: true, protocol: PROTOCOL });
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
