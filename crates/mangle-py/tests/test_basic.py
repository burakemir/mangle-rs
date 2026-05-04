"""Tests for the mangle Python bindings."""
import datetime as dt

import pytest

import mangle


def test_module_attrs():
    assert hasattr(mangle, "__version__")
    assert hasattr(mangle, "Program")
    assert hasattr(mangle, "Name")
    assert hasattr(mangle, "MangleError")
    assert hasattr(mangle, "eval")


def test_eval_simple():
    rows = mangle.eval("p(1). p(2). q(X) :- p(X).", query="q(X)")
    rows = sorted(r[0] for r in rows)
    assert rows == [1, 2]


def test_eval_filter_constant():
    rows = mangle.eval(
        'route("GET", "/a"). route("POST", "/b"). route("GET", "/c").',
        query='route("GET", X)',
    )
    paths = sorted(r[1] for r in rows)
    assert paths == ["/a", "/c"]


def test_program_query_and_relations():
    prog = mangle.Program("p(1). p(2). q(X) :- p(X).")
    rels = set(prog.relations())
    assert {"p", "q"}.issubset(rels)
    rows = sorted(r[0] for r in prog.query("q"))
    assert rows == [1, 2]


def test_program_insert_retract():
    prog = mangle.Program("p(1).")
    assert prog.insert("p", [2]) is True
    rows = sorted(r[0] for r in prog.query("p"))
    assert rows == [1, 2]
    assert prog.retract("p", [1]) is True
    rows = sorted(r[0] for r in prog.query("p"))
    assert rows == [2]


def test_parse_error_raises_mangle_error():
    with pytest.raises(mangle.MangleError):
        mangle.eval("this is not mangle source")


def test_name_constant():
    n = mangle.Name("/role/admin")
    assert str(n) == "/role/admin"
    assert n == mangle.Name("/role/admin")
    with pytest.raises(ValueError):
        mangle.Name("no-leading-slash")


def test_name_round_trip():
    rows = mangle.eval(
        "role(/role/admin). role(/role/user). who(R) :- role(R).",
        query="who(R)",
    )
    names = sorted(str(r[0]) for r in rows)
    assert names == ["/role/admin", "/role/user"]
    # Each row's element is a Name, not a plain str.
    for row in rows:
        assert isinstance(row[0], mangle.Name)


def test_string_distinct_from_name():
    rows = mangle.eval('s("hello").', query="s(X)")
    assert rows[0][0] == "hello"
    assert isinstance(rows[0][0], str)
    assert not isinstance(rows[0][0], mangle.Name)


def test_multi_unit():
    a = "Package a! p(1). p(2)."
    b = "Use a! q(X) :- a.p(X)."
    prog = mangle.Program.from_units([a, b])
    rows = sorted(r[0] for r in prog.query("q"))
    assert rows == [1, 2]


def test_float_value():
    rows = mangle.eval("f(1.5). f(2.5).", query="f(X)")
    vals = sorted(r[0] for r in rows)
    assert vals == [1.5, 2.5]


def test_compound_list():
    # List values flow through as Python lists.
    rows = mangle.eval("l([1, 2, 3]).", query="l(X)")
    assert rows[0][0] == [1, 2, 3]


def test_insert_creates_relation():
    prog = mangle.Program("p(1).")
    # Inserting into a fresh relation auto-creates it.
    assert prog.insert("brand_new", [42]) is True
    assert "brand_new" in prog.relations()
    rows = prog.query("brand_new")
    assert rows == [[42]]
