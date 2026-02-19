# mangle-server

HTTP server for evaluating [Mangle](https://codeberg.org/TauCeti/mangle-rs) programs. Wraps the Rust `mangle-driver` compilation and execution pipeline behind a JSON API.

## Usage

```bash
cargo run -p mangle-server -- [OPTIONS]
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--port <PORT>` | `8090` | Port to listen on |
| `--programs-dir <DIR>` | none | Directory of `.mg` files to load on startup (program name = file stem) |

### Example

```bash
# Start with pre-loaded programs
cargo run -p mangle-server -- --port 8090 --programs-dir ./programs/
```

## API

All endpoints accept and return `application/json`. On error, the response body is `{ "error": "<message>" }` with an appropriate HTTP status code (400, 404, or 500).

### POST /programs

Load a named program. The source is compiled to extract predicate names and stored for later querying.

```bash
curl -X POST http://localhost:8090/programs \
  -H 'Content-Type: application/json' \
  -d '{"name": "social", "source": "friend(\"alice\", \"bob\"). friend(\"bob\", \"carol\")."}'
```

**Response:**
```json
{ "name": "social", "predicates": ["friend"] }
```

### GET /programs

List all loaded programs and their predicates.

```bash
curl http://localhost:8090/programs
```

**Response:**
```json
{ "programs": [{ "name": "social", "predicates": ["friend"] }] }
```

### POST /query

Query a relation from a loaded program. The program is recompiled and executed on each query.

```bash
curl -X POST http://localhost:8090/query \
  -H 'Content-Type: application/json' \
  -d '{"program": "social", "query": "friend(X, Y)"}'
```

**Response:**
```json
{ "results": [["alice", "bob"], ["bob", "carol"]] }
```

The `query` string must be a valid Mangle atom (e.g. `predicate(X, Y)`). The predicate name is extracted to determine which relation to scan. All tuples from that relation are returned.

### POST /eval

Compile and execute ephemeral source without storing it. Useful for one-off evaluation.

```bash
curl -X POST http://localhost:8090/eval \
  -H 'Content-Type: application/json' \
  -d '{
    "source": "edge(1,2). edge(2,3). path(X,Y) :- edge(X,Y). path(X,Z) :- edge(X,Y), path(Y,Z).",
    "query": "path(1, X)"
  }'
```

**Response:**
```json
{ "results": [[2], [3]] }
```

If `query` is omitted, all derived facts across all relations are returned.

## Value encoding

Mangle values map to JSON as follows:

| Mangle | JSON | Example |
|--------|------|---------|
| `Value::Number(n)` | number | `42` |
| `Value::String(s)` | string | `"hello"` |
| `Value::Null` | null | `null` |

Result tuples are returned as arrays of arrays: `[[val, ...], ...]`.

## Mangle syntax notes

String constants in Mangle source must be double-quoted: `greeting("hello")`, not `greeting(hello)`. Unquoted lowercase identifiers are not valid constant syntax. Numbers are unquoted: `p(1, 2)`. Variables start with an uppercase letter: `q(X) :- p(X)`.

## Container image

A Containerfile is provided for building with Podman:

```bash
podman build -t localhost/mangle-server:latest -f rust/server/Containerfile rust/
podman run -p 8090:8090 localhost/mangle-server:latest
```

Pass arguments after the image name:

```bash
podman run -p 8090:8090 -v ./programs:/programs:ro \
  localhost/mangle-server:latest --programs-dir /programs
```
