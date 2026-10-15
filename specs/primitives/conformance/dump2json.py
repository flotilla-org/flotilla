#!/usr/bin/env python3
"""Convert a TLC `-dump` of a Quint-compiled conformance harness to JSON.

Usage: dump2json.py STATES.dump OUT.json

Each dumped state becomes one object mapping variable names to values in
Quint's ITF encoding, so Rust decodes graph nodes and traces alike:
integers {"#bigint": "n"}, sets {"#set": [...]}, functions {"#map": [[k, v]]},
sequences as arrays, records as objects, and sum variants as
{"tag": T, "value": v} with unit payloads {"#tup": []}. Quint compiles a
variant to a one-field TLA+ record keyed by the capitalised constructor;
specification record fields are lower camel case, so the two never collide.
"""
import json
import re
import sys

TOKEN = re.compile(r'\s*(?:(?P<str>"(?:[^"\\]|\\.)*")|(?P<int>-?\d+)|(?P<op>\|->|:>|@@|<<|>>|[\[\](){},])|(?P<word>[A-Za-z_][A-Za-z0-9_]*))')


def tokens(text):
    position, out = 0, []
    while position < len(text):
        if text[position:].strip() == "":
            break
        match = TOKEN.match(text, position)
        if not match:
            raise ValueError(f"unexpected input at {position}: {text[position:position + 40]!r}")
        position = match.end()
        out.append((match.lastgroup, match.group(match.lastgroup)))
    return out


class Parser:
    def __init__(self, text):
        self.items = tokens(text)
        self.index = 0

    def peek(self):
        return self.items[self.index] if self.index < len(self.items) else (None, None)

    def take(self, expected=None):
        kind, value = self.peek()
        if expected is not None and value != expected:
            raise ValueError(f"expected {expected!r}, found {value!r}")
        self.index += 1
        return value

    def value(self):
        kind, token = self.peek()
        if kind == "str":
            self.take()
            return json.loads(token)
        if kind == "int":
            self.take()
            return {"#bigint": str(int(token))}
        if token in ("TRUE", "FALSE"):
            self.take()
            return token == "TRUE"
        if token == "{":
            return {"#set": self.sequence("{", "}")}
        if token == "<<":
            return self.sequence("<<", ">>")
        if token == "[":
            return self.record()
        if token == "(":
            return self.function()
        raise ValueError(f"unexpected token {token!r}")

    def sequence(self, opening, closing):
        self.take(opening)
        items = []
        while self.peek()[1] != closing:
            items.append(self.value())
            if self.peek()[1] == ",":
                self.take(",")
        self.take(closing)
        return items

    def record(self):
        self.take("[")
        fields = {}
        while self.peek()[1] != "]":
            name = self.take()
            self.take("|->")
            fields[name] = self.value()
            if self.peek()[1] == ",":
                self.take(",")
        self.take("]")
        if fields == {"tag": "UNIT"}:
            return {"#tup": []}
        if len(fields) == 1:
            (name, payload), = fields.items()
            if name[0].isupper():
                return {"tag": name, "value": payload}
        return fields

    def function(self):
        self.take("(")
        pairs = []
        while self.peek()[1] != ")":
            key = self.value()
            self.take(":>")
            pairs.append([key, self.value()])
            if self.peek()[1] == "@@":
                self.take("@@")
        self.take(")")
        return {"#map": pairs}


def parse_value(text):
    parser = Parser(text)
    result = parser.value()
    if parser.index != len(parser.items):
        raise ValueError(f"trailing input after value: {parser.items[parser.index:][:3]}")
    return result


def states(dump, prefix=""):
    """Yield each dumped state as {variable: value}."""
    for block in re.split(r"^State \d+:\s*$", dump, flags=re.M)[1:]:
        state = {}
        for assignment in re.split(r"^/\\ ", block.strip(), flags=re.M):
            if not assignment.strip():
                continue
            name, _, value = assignment.partition(" = ")
            state[name.strip().removeprefix(prefix)] = parse_value(value)
        yield state


def main(arguments):
    source, target = arguments
    with open(source) as handle:
        nodes = list(states(handle.read()))
    if not nodes:
        raise SystemExit(f"{source}: no states")
    with open(target, "w") as handle:
        json.dump(nodes, handle)
    print(f"{target}: {len(nodes)} states")


if __name__ == "__main__":
    main(sys.argv[1:])
