# mangle-py

Python bindings for Mangle (Datalog-style logic language), built with PyO3 and maturin.

## Build

```sh
# in crates/mangle-py
uv venv
uv pip install maturin
maturin develop --release
```

Then `import mangle` in Python.

## API (v1)

```python
import mangle

# One-shot evaluation
results = mangle.eval("p(1). p(2). q(X) :- p(X).", query="q(X)")

# Stateful program
prog = mangle.Program("p(1). q(X) :- p(X).")
prog.query("q")              # -> [[1]]
prog.relations()             # -> ["p", "q"]
prog.insert("p", [2])        # add EDB fact (no auto re-derivation)
prog.retract("p", [1])

# Multi-unit
prog = mangle.Program.from_units([unit_a, unit_b])

# Name constants distinct from strings
mangle.Name("/role/admin")
```

## Limitations (v1)

- No automatic re-derivation after `insert`/`retract`. To re-evaluate rules, create a new `Program`.
- Compound `Struct` values map to Python `dict`; field order is not preserved on round-trip.
- WASM/server mode and Python-implemented `Store` backends are not exposed.
