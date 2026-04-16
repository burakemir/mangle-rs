# mangle-wasm

Browser WASM target for the Mangle deductive database language. Compiles the
full Mangle interpreter (parser, analyzer, and evaluator) to
`wasm32-unknown-unknown` for use in web applications.

## Prerequisites

Install [wasm-pack](https://rustwasm.github.io/wasm-pack/installer/):

```sh
curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh
```

## Building

### Dynamic mode

Build a WASM module that accepts both program source and data at runtime:

```sh
wasm-pack build --target web crates/mangle-wasm
```

### Bundled mode (partial evaluation)

Bake a Mangle program into the WASM at compile time. At runtime, only the
input data needs to be supplied:

```sh
MANGLE_PROGRAM='reachable(Y) :- edge(1,Y). reachable(Z) :- reachable(Y), edge(Y,Z).' \
  wasm-pack build --target web crates/mangle-wasm
```

The resulting WASM binary (~780 KB) includes the full Mangle pipeline.

## JavaScript API

### Setup

After building with `wasm-pack`, the generated package is in `pkg/`. Import
and initialize it in your JavaScript:

```html
<script type="module">
  import init, { run_mangle, run_bundled } from './pkg/mangle_wasm.js';

  async function main() {
    await init();  // Initialize the WASM module

    // Now run_mangle and run_bundled are available
  }

  main();
</script>
```

### `run_mangle(source, facts_json) -> string`

Run an arbitrary Mangle program with initial facts.

**Parameters:**
- `source` (string): Mangle source code (rules and/or inline facts).
- `facts_json` (string): JSON object mapping relation names to arrays of
  tuples. Pass `"{}"` for no initial facts.

**Returns:** A JSON string mapping each relation name to its computed tuples.

```js
await init();

// Program with inline facts
const result1 = run_mangle(
  'p(1). p(2). q(X) :- p(X).',
  '{}'
);
console.log(JSON.parse(result1));
// { "p": [[1], [2]], "q": [[1], [2]] }

// Program with external data
const result2 = run_mangle(
  'reachable(Y) :- edge(1, Y). reachable(Z) :- reachable(Y), edge(Y, Z).',
  JSON.stringify({
    edge: [[1, 2], [2, 3], [3, 4], [4, 5]]
  })
);
console.log(JSON.parse(result2));
// { "edge": [[1,2],[2,3],[3,4],[4,5]], "reachable": [[2],[3],[4],[5]] }
```

### `run_bundled(facts_json) -> string`

Run the compile-time bundled program. Only available when built with the
`MANGLE_PROGRAM` environment variable set.

```js
await init();

// Only supply data; the program was baked in at build time
const result = run_bundled(JSON.stringify({
  edge: [[1, 2], [2, 3], [3, 4]]
}));
console.log(JSON.parse(result));
// { "edge": [[1,2],[2,3],[3,4]], "reachable": [[2],[3],[4]] }
```

## Data Format

### Input facts

A JSON object where keys are relation names and values are arrays of tuples
(each tuple is an array of values):

```json
{
  "edge": [[1, 2], [2, 3], [3, 4]],
  "name": [["alice"], ["bob"]]
}
```

Supported value types:
- **Integers**: `42`, `-1`
- **Floats**: `3.14`
- **Strings**: `"hello"`
- **Names**: `{"@name": "/alice"}` — Mangle names (the `/foo/bar` syntax) are
  encoded as a tagged object so they are distinguishable from plain strings
  on both input and output.

### Output

The return value is a JSON string with the same structure: an object mapping
every relation (both input and derived) to its tuples.

```json
{
  "edge": [[1, 2], [2, 3]],
  "reachable": [[2], [3]]
}
```

## Complete Example

Save this as `index.html` alongside the `pkg/` directory produced by
`wasm-pack build --target web crates/mangle-wasm`:

```html
<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>Mangle WASM</title></head>
<body>
  <h1>Mangle in the Browser</h1>
  <textarea id="source" rows="8" cols="60">
edge(1, 2). edge(2, 3). edge(3, 4). edge(4, 5).
reachable(Y) :- edge(1, Y).
reachable(Z) :- reachable(Y), edge(Y, Z).
  </textarea>
  <br>
  <textarea id="facts" rows="4" cols="60">{}</textarea>
  <br>
  <button id="run">Run</button>
  <pre id="output"></pre>

  <script type="module">
    import init, { run_mangle } from './pkg/mangle_wasm.js';

    async function main() {
      await init();

      document.getElementById('run').addEventListener('click', () => {
        const source = document.getElementById('source').value;
        const facts = document.getElementById('facts').value;
        try {
          const result = run_mangle(source, facts);
          document.getElementById('output').textContent =
            JSON.stringify(JSON.parse(result), null, 2);
        } catch (e) {
          document.getElementById('output').textContent = 'Error: ' + e.message;
        }
      });
    }

    main();
  </script>
</body>
</html>
```

## Using with npm/bundlers

For use with bundlers (webpack, vite, etc.), build with the `bundler` target:

```sh
wasm-pack build --target bundler crates/mangle-wasm
```

Then import in your JavaScript/TypeScript:

```js
import init, { run_mangle } from 'mangle-wasm';

await init();
const result = run_mangle('p(1). q(X) :- p(X).', '{}');
```

## Using with Node.js

```sh
wasm-pack build --target nodejs crates/mangle-wasm
```

```js
const { run_mangle } = require('./pkg/mangle_wasm.js');
const result = run_mangle('p(1). q(X) :- p(X).', '{}');
console.log(JSON.parse(result));
```
