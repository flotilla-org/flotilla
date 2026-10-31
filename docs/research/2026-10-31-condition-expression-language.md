# One condition language: a CEL subset for flotilla and binnacle

**Date:** 2026-10-31
**Status:** complete (research; no code changes). The grammar and semantics below are a worked proposal for the owner to rule on, not a built spec. Names (`rigging-condition`, the `address` type) are placeholders until that ruling.
**For:** flotilla#4195, part of mission flotilla#4105. Raised by ruling 3 of the verb-document grilling (flotilla#4182).

## Question

Should flotilla's condition grammar (`crates/flotilla-address/src/condition.rs`, ADR 0066 §4) become a defined subset of a known expression language, rather than grow one operator at a time? If so, which language, and what exactly is the subset? It has to serve:
- `wait` leaves;
- message conditions;
- promise-kind rules;
- the `/tables/{table}/rows` `where` parameter;
- verb `when` conditions, evaluated by binnacle clients in Rust, TypeScript and C and re-checked by the daemon;
- binnacle's View Spec filters.

The language should live in rigging (ADR 0068).

## Answer

**Yes. Adopt a strict subset of CEL**, the Common Expression Language. The subset is chosen so that, over it, three languages agree:
- CEL's evaluation rules;
- SQLite's `WHERE` (three-valued `NULL` logic);
- SPARQL 1.1 `FILTER`, where an error means the solution is dropped.

The subset is the **intersection** of the three. It is written in CEL syntax because CEL is the only candidate that has all three of:
- a normative evaluation spec;
- an expression-level conformance suite;
- maintained evaluators in the languages flotilla needs (§3).

In one paragraph: a condition is a boolean CEL expression over the named, typed fields of one row. It allows:
- comparisons of a field with a literal;
- `in` over a literal list;
- `&&`, `||` and `!`;
- `has(field)`;
- one extension function, `field.under("…")`, for hierarchical addresses.

A field with no value is **absent**, and any comparison that reads it gives a third value, **U** (unknown). `&&`, `||` and `!` use the strong Kleene tables, which CEL, SQL and SPARQL already share. What U means is the consumer's decision:
- a filter keeps only true rows;
- `wait` fires only on true;
- a message withdraws only on false;
- a verb greys out on anything but true.

Literals are typed. Each is checked against the field's declared type before evaluation, so no evaluator ever converts types at run time.

Rigging owns the language:
- grammar;
- AST;
- canonical printer;
- type checker;
- three-valued evaluator;
- SQLite translation;
- a conformance corpus.

Flotilla owns:
- field resolution (descriptors, the leaf vocabulary, staleness);
- the subject layer (`<address> <expression>`, `$var` templates);
- address validation;
- the legacy decoders.

The ADR 0066 spelling stays decodable for one generation. A legacy condition is recognised because its expression starts with `.`, and it is converted to the new form on decode.

### Why not the others

Each fails a test that CEL passes:
- **SQL `WHERE`.** It has the right logic, but SQLite's loose typing (type affinity) cannot be reproduced in a client evaluator, and there is no evaluator outside SQLite. Use it as the **target** of translation, not as the language.
- **SPARQL `FILTER`.** Its semantics are rigorous and close to what we need. But it has no duration comparison in core, no normative AST, and a whole-query (not expression-level) test suite. Its only C implementation, rasqal, last released in 2014. **Borrow its rules, not its syntax** (§7).
- **exprtk.** It has loops and assignment, so it is not guaranteed to terminate. It has no null, no timestamps and no spec other than its source. Perspective itself does not use exprtk for filters.
- **JSONLogic.** Its `==` is JavaScript's loose equality, and its reference implementation is the spec.
- **FEEL.** It is a full language with user functions, and unfamiliar outside BPM.
- **OData `$filter`.** It has no maintained Rust or JS evaluator, and `null eq null` is true, which is not SQL's rule.
- **Kubernetes selectors.** No OR and no ordering.
- **AIP-160.** It describes itself as "fuzzy" and points to CEL for deterministic evaluation. `OR` binds tighter than `AND`. It has no `in`.

## 1. Where flotilla is today

This section summarises a read of `flotilla` at `34d7ad85`.

### Two condition models

**(A) The ADR 0066 grammar.** `Condition { address, path, operator, literal: String }` and `Template` (`crates/flotilla-address/src/condition.rs:105-111`, `:193-199`). Its users:
- engine Message outcomes and delivery conditions (`crates/flotilla-state-machines/src/machines/message.rs:66-74`, `:176-179`);
- promise kinds (`crates/flotilla-state-machines/src/kinds.rs`);
- workflow-template turn rules and exits (`crates/flotilla-resources/src/workflow_template.rs:102-140`);
- the tables `where` (`crates/flotilla-daemon/src/server/tables.rs:175-217`).

**(B) The older `flotilla_protocol::Leaf`** (`crates/flotilla-protocol/src/leaf.rs:6-12`).
- `wait --for` takes Leafs directly. Repeating `--for` gives OR (`crates/flotilla-cli/src/cli/args.rs:170-187`), the only disjunction in the system.
- Most (A) conditions are converted to a Leaf (`crates/flotilla-core/src/primitives/view.rs:77-121`) and evaluated by `evaluate_leaf` (`crates/flotilla-resources/src/leaf.rs`).

### Their semantics already disagree

1. **Missing values.**
   - The leaf and promise evaluators return **Unknown** for an absent, stale or missing value, and `!=` on a missing field is Unknown (`leaf.rs:138-169`, `kinds.rs:189-215`).
   - The engine tables `where` pushes into SQLite, where a NULL cell drops the row for every operator, including `!=` (`crates/flotilla-schema-sqlite/src/table_read.rs:93-125`).
   - The resource-kind `where` drops bools and nulls from its index entirely (`crates/flotilla-store/src/query.rs:521-528`).
2. **Literal typing is decided three different ways:**
   - by **path name** in leaves (`bind_literal`, `leaf.rs:171-179`: only `.latest-claim.claimed-at` and `.updated-at` are timestamps);
   - by **column type** in tables (`crates/flotilla-daemon/src/server/tables/decided.rs:57-72`);
   - by a **hand-written vocabulary** in promise kinds (`kinds.rs:59-115`).
3. **Enums have three spellings:**
   - Rust `Debug` names for phases (`Landed`, `leaf.rs:227`);
   - lowercase words for CR and issue states (`leaf.rs:322-327`);
   - short stored codes on engine `code` columns (`table_read.rs:24-38`).

   Ordering on a `code` column is lexicographic on the codes.
4. **`in` exists only elsewhere.** It appears in the index-declaration DSL (`when(state in [...])`, `crates/flotilla-schema-derive/src/index.rs:6-16`), which uses a different `Condition` type.
5. **`under` has no SQL path yet.**
   - `Address::is_under` and `descendant_bounds` exist (`crates/flotilla-address/src/address.rs:214-227`), but only tests call them.
   - The resource store has a prefix-range query (`crates/flotilla-store-sqlite/src/sqlite.rs:1196-1205`), but `where` cannot reach it.
   - Address columns are described on the wire as plain `string` (`crates/flotilla-schema/src/frictionless.rs:40-98`), so a client cannot tell where `under` applies.

### Stored forms that must stay decodable (ADR 0047)

| Where | Form |
|---|---|
| Engine Message outcome and delivery condition | Interned canonical text in the `ad` domain (`crates/flotilla-schema/src/traits.rs:314-326`). The message gate is keyed by the same text (`message.rs:734-736`). |
| Engine `ConvoySpec.kinds: Frozen<PromiseKinds>` | Content-addressed canonical CBOR holding `Template` text (`convoy.rs:264-265`, `crates/flotilla-schema/src/frozen.rs:16-31`). |
| Old-store workflow templates and frozen convoy workflow snapshots | Legacy pre-0066 text `$cr.state == merged` (`workflow_template.rs:252-273`). |
| Old-store Message and Convoy status Leafs | serde JSON structs with an externally tagged `LeafAddress` (`resources/src/message.rs:116-147`, `resources/src/convoy.rs:1235-1268`). |

No structured (AST) form of a condition exists anywhere. Conditions cross CBOR only as text.

### Binnacle's View Spec filters

At binnacle `074be4c`, these are a separate, structured model (`crates/binnacle-core/src/view.rs:175-341`, `web/src/source.ts:36-90`, `CONTEXT.md` "Condition"):
- a `Condition` names one column (`key`) and holds one `ConditionKind`, with a per-condition `not`:
  - `Text(TextQuery)`;
  - `OneOf(Vec<Value>)`, where `Value::Blank` differs from empty text;
  - `Compare(Comparison)`: equal, notEqual, less, atMost, greater, atLeast or between, over a number or an epoch-millisecond date;
  - `Native(String)`, opaque text in the source's own syntax.
- Conditions are ANDed in a `ConditionTree::All`, which is `#[non_exhaustive]`.
- Its logic is **two-valued**: "a blank, a NaN, or a value of the other kind never passes, so `not` keeps it" (`view.rs:219-221`).

## 2. The candidates

Every claim here is cited in **Sources**. The survey read the specs and source at the commits listed there, on 2026-10-31.

| | CEL | SQL `WHERE` (SQLite) | SPARQL 1.1 `FILTER` | AIP-160 | OData `$filter` | JSONLogic | FEEL | exprtk | k8s selectors |
|---|---|---|---|---|---|---|---|---|---|
| Comparisons | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ (JS loose `==`) | ✓ | ✓ | `=` `!=` only |
| `in` | ✓ list, map key | ✓ (empty: false, even for NULL) | ✓ `IN` = chain of `=` | ✗ (`:` has) | ✓ | ✓ array or substring | ✓ plus ranges | substring only | ✓ `in`/`notin` |
| and/or/not | ✓ Kleene, errors absorbed | ✓ Kleene over NULL | ✓ Kleene over error | ✓ (OR binds tighter than AND) | ✓ Kleene over null | returns an operand | ✓ Kleene over null | ✓ | AND only |
| Missing field | map key: error; `has()` tests presence | NULL | unbound: error; `BOUND()` | entry skipped, even for `!=` | null | `var`: null | (not verified) | n/a | `!=` and `notin` **match** an absent key |
| Timestamps | `timestamp("…")`, ns | none (TEXT/INT by convention) | `xsd:dateTime` | RFC 3339 string, coerced | ✓ | ✗ | ✓ | ✗ | ✗ |
| Durations | `duration("…")`, h and smaller | ✗ | extension only | `"20s"`, coerced | ✓ ISO 8601 | ✗ | ✓ (two kinds) | ✗ | ✗ |
| Enums | proto ints only | ✗ | IRIs or literals | strings, validated | `Ns.Enum'X'` | ✗ | allowed values | ✗ | ✗ |
| `under` | extension function (no new operators) | plain SQL (range) | IRI-named function, or `STRSTARTS` | function | service function | custom op | function | user function | ✗ |
| Grammar | EBNF + ANTLR | railroad diagrams + `parse.y` | EBNF, LL(1) | EBNF | ABNF | JSON | EBNF | none | informal |
| Evaluation rules | ✓ detailed | ✓ docs | ✓ normative tables | "fuzzy", should/may | ✓ prose | reference implementation | ✓ tables | the header file | prose |
| Terminates | ✓ stated | ✓ (no recursive CTE) | ✓ (without `EXISTS`) | ✓ | ✓ | ✓ | ✗ (user functions) | ✗ (loops) | ✓ |
| Rust | `cel` 0.15 (runs the suite) | target | oxigraph, sophia | small crates | ✗ | datalogic-rs | dsntk | ✗ | client libraries |
| TS/JS | `@bufbuild/cel` (beta, runs the suite), `@marcbachmann/cel-js` | target | Comunica | ✗ | parser from 2018 | json-logic-js (stale) | feelin | ✗ | client libraries |
| C / wasm | cel-cpp (heavy); no pure C | SQLite | rasqal (last release 2014) | ✗ | ✗ | various | ✗ | C++ header | n/a |
| Conformance suite | **expression-level, about 2,771 cases** | not for expressions | whole queries | ✗ | grammar only (813) | `tests.json` (about 278) | DMN TCK | ✗ | ✗ |
| Normative AST | protobuf `ParsedExpr`/`CheckedExpr` | ✗ | ✗ (algebra is informative) | (aip-go uses CEL's) | ✗ | the JSON is the AST | ✗ | ✗ | `LabelSelector` struct |
| Familiarity | high (Kubernetes, Envoy, GCP) | universal | niche | GCP users | MS/SAP | form tools | BPM | finance/maths | very high |

### The finding that decides the shape

For `and`, `or` and `not`, five of these languages use the **same strong Kleene truth table**. They differ only in what the third value is called:

| | Third value U |
|---|---|
| SQL | NULL |
| SPARQL | error |
| CEL | error or unknown |
| OData | null |
| FEEL | null |

```
T || U = T     F || U = U     U || U = U
F && U = F     T && U = U     U && U = U
!U = U
```

CEL says outright why it chose these rules: so that its "boolean operators [can] be mapped to indexed queries, and align better with SQL semantics" (langdef, Logical Operators). SPARQL §17.2 gives the same table for errors.

They **differ** in only two respects:
- **which leaf comparisons produce U;**
- **what the top level does with U.**

Fix both the same way across the languages and they translate one-to-one:
- **a missing field gives U, and nothing else does** (type errors are caught before evaluation);
- **a filter treats a top-level U as "no match"**, which is SPARQL's FILTER rule (§17) and SQL's `WHERE` rule.

Kubernetes label selectors are the one outlier: an absent key **matches** `!=` and `notin`. AIP-160 decided the opposite in 2025: "This applies even when the comparison is a `!=`". This proposal follows SQL, SPARQL, CEL, AIP-160 and flotilla's own leaf evaluator.

## 3. Why CEL, and how strict a subset

CEL is the base because only it has all of the following:
1. **A written grammar and evaluation rules.** langdef has an EBNF, precedence, the logical-operator error rules, the ordering of timestamps and durations, and the limits.
2. **Stated determinism and termination.** "a CEL expression will deterministically evaluate to either a value or an error", and "CEL programs cannot loop forever".
3. **An expression-level conformance suite.** `tests/simple/testdata/*.textproto` at cel-spec `21ec6f4e` holds about 2,771 data-driven cases, sectioned so an implementation can take a subset (`basic`, `comparisons`, `logic`, `fields`, `lists`, `timestamps`, …). Two independent implementations run it:
   - the Rust `cel` crate, 0.15.0;
   - `@bufbuild/cel` in TypeScript, 0.6.1, beta.
4. **Familiarity.** Kubernetes uses CEL for CRD validation rules and admission policies, so operators who write `wait` conditions or git-mastered verbs have likely seen it. AIP-160 points to CEL for deterministic evaluation, and Google's own AIP-160 Go library (`einride/aip-go`) parses filters *into* CEL's AST.
5. **An extension mechanism that suits `under`.** CEL forbids new operators but allows extension functions, including receiver style: `inbox.under("project:flotilla/convoy:demo")`.

### Where plain CEL does not fit flotilla, and what the subset does

| CEL as specified | Problem for flotilla | Subset rule |
|---|---|---|
| Missing map key is an error, but a present `null` compares: `null != "a"` is **true** | That diverges from SQL, where `NULL <> 'a'` is NULL | No `null` literal. An absent value is an absent key, never a `null` value. Presence is tested only with `has()`. |
| `x in []` is an error for a missing `x`, but SQLite says `NULL IN ()` is false | The two evaluators disagree | An empty list is a check error. |
| Ternary, arithmetic, macros, strings, regex, bytes, uint, maps and lists as values | Not needed. Each one adds SQL translation and cross-language risk. | Excluded (§4.4). |
| Gradual typing; heterogeneous `==` is false at run time | A client and the server could disagree on an ill-typed condition | Fully checked against declared field types before evaluation. A condition that does not check is refused at write time. |
| Enums are protobuf ints | flotilla's enums are closed string sets | A `string` field with a declared value set. The checker requires each literal to be in the set. |
| Error and unknown are separate | The subset has only one source of U | Every runtime error in the subset comes from an absent value, so a stock CEL evaluator's "error" is exactly U. |
| Comments, single quotes, raw and triple-quoted strings, many escapes | A stored condition needs exactly one spelling (ADR 0066 §4) | Strict parsing accepts canonical text only. Lenient input accepts any CEL spelling of the same AST. |

**The compatibility promise.** Take any expression the subset accepts, with its fields declared as in §4.3. Bind them in a conforming CEL evaluator, under these conventions:
- the row is a map with absent fields left out;
- every intermediate map of a dotted path is present, even when the sub-structure behind it is missing (a stock CEL evaluator errors on `has(a.b)` when `a` is missing). The oracle adapter builds these maps; rigging's own `Row` trait never needs them (§4.5);
- addresses and enums are strings;
- timestamps are `google.protobuf.Timestamp`;
- `under` is registered as in §4.6.

The evaluator then returns `true` where this spec says T, `false` where it says F, and an error where it says U. The promise covers rows whose fields are present or absent. It does not cover *unknown* fields (§4.5), which stock CEL can only model through cel-go's partial evaluation, so corpus cases with unknown fields are checked against this spec's tables alone. This lets the stock `cel` crate act as a **differential oracle** in rigging's tests, without being a runtime dependency.

## 4. The subset: grammar and semantics

### 4.1 Layers

```
 ┌──────────────────────────────────────────────────────────────────────┐
 │ flotilla: subject layer                                              │
 │   <address> <expression>          wait leaves, message conditions    │
 │   $cr <expression>                templates, promise kinds           │
 │   when: [<expression>, …]         verbs (row-bound, no subject)      │
 │   where=<expression>              tables (row-bound, repeatable)     │
 ├──────────────────────────────────────────────────────────────────────┤
 │ rigging-condition: expression layer                                  │
 │   expression over ONE row of named, typed fields → T | F | U         │
 │   grammar · AST · canonical printer · checker · evaluator · SQL      │
 ├──────────────────────────────────────────────────────────────────────┤
 │ field resolution (flotilla, binnacle sources)                        │
 │   schema: field → type, from descriptors or the leaf vocabulary      │
 │   row:    field → value | absent | unknown (unobserved or stale)     │
 └──────────────────────────────────────────────────────────────────────┘
```

The expression never names its subject.
- **Subject-bound conditions:** the address (or the `$var` that becomes one) selects which row the expression reads, and the subject's kind decides the schema.
- **Combining several subjects** stays a consumer's job, above the language. `wait --for` repeated means "any of these (subject, expression) pairs", as it does today. The expression language has no cross-subject joins.

### 4.2 Grammar

The notation is CEL's: `[]` optional, `{}` repeated, `|` alternatives. Every production is a restriction of CEL's EBNF (langdef, Syntax). Precedence and associativity are CEL's.

```
Condition   = Or ;
Or          = And { "||" And } ;
And         = Unary { "&&" Unary } ;
Unary       = "!" Atom
            | Atom ;
Atom        = "(" Or ")"
            | "has" "(" Path ")"
            | Field "." "under" "(" Operand ")"
            | Field Relop Operand
            | Field "in" "[" Operand { "," Operand } "]"
            | StringLit "in" Field                 (* key in map, member of list *)
            | Field                                (* a bool field *)
            | BoolLit ;
Relop       = "==" | "!=" | "<" | "<=" | ">" | ">=" ;
Operand     = Literal | Variable ;
Field       = Path [ "[" StringLit "]" ] ;            (* index only on a map field *)
Path        = Segment { "." Segment } ;
Segment     = SELECTOR | "`" EscChar { EscChar } "`" ;
Variable    = IDENT { "." Segment } ;              (* declared bound variables: row, param, revision … *)
Literal     = StringLit | IntLit | FloatLit | BoolLit
            | "timestamp" "(" StringLit ")"
            | "duration" "(" StringLit ")" ;
```

The lexical rules are CEL's.
- `SELECTOR` is `[_a-zA-Z][_a-zA-Z0-9]*`, excluding keywords.
- `IDENT` also excludes CEL's reserved words.
- `EscChar` is CEL's escaped-identifier set: letters, digits and `_ . - / space` (`CEL.g4` `ESC_IDENTIFIER`; cel-spec `fields.textproto` section `quoted_map_fields`).
- `StringLit`, `IntLit`, `FloatLit` and `BoolLit` are CEL's `STRING_LIT`, `INT_LIT`, `FLOAT_LIT` and `BOOL_LIT`, without raw, bytes or triple-quoted forms in strict mode.

Restrictions beyond CEL:
- **A field is always on the left** of a comparison or `in`. The one exception is a string key on the left of `in` with a map or list field on the right. This is AIP-160's rule ("field names must appear on the left-hand side"), and it keeps SQL translation a direct mapping.
- **No leading `.`** on a field, though CEL allows it. This is what tells new text from the legacy spelling (§5).
- **A `Variable` is never a field.** `Variable` and `Field` look alike to the parser; the checker tells them apart by the schema. A name declared as a bound variable (§4.3) can't also be a field, and the variable is replaced by its value before evaluation.
- **`has()` takes a path with no index,** as CEL's `has(e.f)` requires (langdef, Field Selection). A map key's presence is `"k" in m`.
- **Limits.**
  - At most 32 terms in one `&&` or `||` run.
  - At most 32 list elements.
  - At most 12 nested parentheses or `!`.
  - At most 12 segments in a field.
  - At most 4,096 bytes of text.

  These are CEL's *minimum* supported limits (langdef, Syntax), so any conforming CEL implementation accepts every condition.

  **The parser enforces the limits,** before any schema is read, so an over-limit `where` is always `invalid_filter`, whatever the table. Errors that need the schema (unknown field, wrong literal type, ordering an enum) come from the checker as `unknown_column` or `type_mismatch`, the codes `docs/tables.md` already lists.

### 4.3 Types and the schema

The checker reads a **schema**: each field path maps to a type and a nullable flag. It may also declare bound variables. The types:

| Type | Values | Literals | Operators |
|---|---|---|---|
| `bool` | true, false | `true` `false` | `==` `!=`, bare field, `!` |
| `int` | i64 | `IntLit` | all six relations, `in` |
| `number` | f64, never NaN | `IntLit` or `FloatLit` | all six relations, `in` |
| `string` | Unicode text | `StringLit` | all six relations, `in` |
| `enum{v…}` | one of the listed strings | a `StringLit` in the set | `==` `!=` `in` (**no ordering**) |
| `address` | canonical ADR 0066 address text | a `StringLit` holding a canonical address | `==` `!=` `in` `under` (**no ordering**) |
| `timestamp` | instant, microsecond precision, UTC | `timestamp("…")` | all six relations, `in` |
| `duration` | signed span, microsecond precision | `duration("…")` | all six relations, `in` |
| `map(string, T)` | string-keyed map of scalars | — | `"k" in m`, `m["k"]` as a field of type T |
| `list(T)` | list of scalars | — | `"x" in l` (T is `string`, `enum` or `address`) |

Checker rules:
- **Every field must be declared.** An unknown field is a check error. That is a `type_mismatch` or `unknown_column` for `where`; a broken verb at load (#4182 ruling 10); a refused `wait`.
- **Literals must match the field's type,** including enum membership and canonical address text. `int` against a `FloatLit` is an error. `number` takes either.
- **Timestamps.** `timestamp("…")` takes an RFC 3339 string. More than six fractional digits is an error: storage is microseconds (`descriptor.rs:228-229`), so pushdown compares exactly.
- **Durations.** `duration("…")` takes CEL's duration syntax with units `h m s ms us`. `ns` is an error, as are days, which CEL itself does not support.
- **Ordering is refused** on `bool`, `enum` and `address`. Enum order is not declared anywhere, and today's code-column ordering is an accident of the short codes.
- **`in` needs a non-empty list** of operands of the field's type.
- **Bound variables.**
  - `row.<column>` in a verb's pick `where` (#4182 ruling 8);
  - `param.<name>`;
  - the promise-kind `revision` term (`kinds.rs:59-115`).

  The consumer declares these with their types. They are substituted with literal values before evaluation and before SQL translation, as constant folding. An unbound variable at evaluation time is a consumer bug, not U.
- **The whole expression must be `bool`.**

### 4.4 What the subset leaves out, and why

- **The `null` literal and `== null`:** absence is `has()`.
- **Ternary `?:`, arithmetic, unary minus on fields, field-to-field comparison:** none of the consumers needs them. `row.x` covers the right-hand-side variable case.
- **`now`:** it is not deterministic across a client and the server. A relative-time condition such as "updated in the last hour" binds a timestamp variable when the consumer evaluates it, and says which clock it used.
- **String functions** (`contains`, `startsWith`, `matches`), case folding and regex. binnacle's Text Query stays outside the shared language (§6). `contains` and `startsWith` are the first candidates to add later: both are standard CEL and translate to `instr`/`substr`.
- **Macros other than `has`** (`all`, `exists`, `map`, `filter`): these are the only source of super-linear cost in CEL (langdef, Performance).
- **`uint`, `bytes`, message construction and comments in strict text.**

### 4.5 Evaluation

The evaluator returns `eval(e, row) ∈ {T, F, U}`. Rows provide `get(path) → present(value) | absent | unknown`, looked up by the field's **full declared path**. A dotted path names one field; it is not a walk through nested maps, so a missing sub-structure (no latest claim, for `` `latest-claim`.disposition ``) is simply that field being absent or unknown.

- **A field is absent** when the row knows it has no value:
  - the field's cell is NULL;
  - a `number` is NaN;
  - a map has no such key.
- **A field is unknown** when the row cannot say whether it has a value:
  - the subject has no record, or has not been observed (`message_conditions.rs:77-83`);
  - the observation is stale (`leaf.rs:318`, `:363-371`).

  Only leaf subjects produce unknown. Table rows are stored values, so for them every field is present or absent.
- **`Field op v`:** U if the field is absent or unknown. Otherwise the comparison of two values of one type:
  - `int` and `number` compare on the number line;
  - `string` compares by **Unicode code point**, which is CEL's ordering, and SQLite `BINARY` on UTF-8, and Rust `str` ordering. JavaScript's `<` compares UTF-16 code units, so a TS evaluator must compare by code point (it differs for characters outside the BMP);
  - `timestamp` and `duration` compare by microsecond count;
  - `enum` and `address` compare as strings, for equality only.
- **`Field in [v…]`:** U if the field is absent or unknown, otherwise T if it equals any element, else F. The list holds only literals, so its elements are never U.
- **`"k" in Field`:** U if the field (the map or list) is absent or unknown, otherwise membership.
- **`has(Path)`:** T if present, F if absent, **U if unknown**. So `!has(f)` on a stale observation is U, not T, and rewrites such as §6's `!has(f) || …` keep "unobserved is not failed" for `wait` and messages.
- **Bare bool field:** its value, or U if absent or unknown. `BoolLit` is its value.
- **`!`, `&&`, `||`:** the Kleene tables in §2. They are commutative: evaluation order is free and a short-circuit never changes the result (CEL langdef, Logical Operators).
- **Determinism.** The result depends only on the expression and the row.
- **Termination.** Evaluation is linear in the expression's size: no loops, no recursion, at most 32 list elements per `in`.

What a consumer does with U:

| Consumer | T | F | U |
|---|---|---|---|
| `where` filter, binnacle filter | keep | drop | drop |
| `wait` leaf | fire | wait | wait (as today, `ThreeValue::Unknown`) |
| Message delivery condition | deliver | **withdraw** | hold ("unobserved is not failed", `message.rs:764-770`) |
| Message outcome expectation | met | not met | not met yet |
| Promise rule | kept or ready | not | unknown (`kinds.rs:189-215`) |
| Verb `when` (client) | enabled | greyed, with reason | greyed, with reason "unknown: `<field>`" |
| Verb `when` (server re-check) | run | `not_applicable` | `not_applicable` |

The engine sweep currently writes an Unknown outcome into the gate as `false` (`crates/flotilla-core/src/primitives/sweep.rs:253-257`). The gate keeps two values, but the language returns three. This is filed as #4204.

**Reasons.** For `when`, the evaluator should report the first top-level conjunct that is not T, in canonical text, so the "failing condition as its reason" of #4182 ruling 3 comes for free. `when: [a, b]` means `a && b`.

### 4.6 `under`

`f.under("P")` is defined in the language itself, not supplied by flotilla, because every client evaluator has to agree on it:

- `f` must have type `address`. `P` is a string literal holding a canonical address, or a bound variable of type `address` (a pick's `vessel.under(row.convoy)`), which is checked when it is bound.
- The result is U if `f` is absent or unknown. Otherwise it is T if and only if `f == P`, or `f` starts with `P + "/"`.

This is ADR 0066 §4's segment-boundary rule: `…/convoy:demo2` is not under `…/convoy:demo`. It needs no address parsing at evaluation time, because a canonical value never contains a bare `/` (ADR 0066 §4). The definition depends on the canonical encoding alone, which is why the checker requires canonical literals. Lenient input such as a PR URL or mixed case is resolved to canonical text before checking, through `flotilla_address::resolve`, as today.

The equivalent CEL, for the oracle: `f == P || f.startsWith(P + "/")`.

**This differs from the brief's suggestion**, which listed `under` among what stays in flotilla. What stays in flotilla is:
- which columns are addresses (the `address` type, #4108's role);
- validating and resolving address literals.

The function's meaning moves to rigging. Otherwise binnacle's C and TS clients could not evaluate a `when` that uses it.

### 4.7 Canonical text

Print-then-parse returns exactly the AST, and parse-then-print returns exactly the canonical text, as ADR 0066 §4 requires of today's grammar. The interned key, the message gate key and the frozen bytes are all the canonical print (§5).

- **Spacing.** One space on each side of `==` `!=` `<` `<=` `>` `>=` `in` `&&` `||`. None after `!`, inside brackets or parentheses, or around `.`. Lists print as `[a, b]`.
- **Strings** are double-quoted with JSON escaping exactly as `serde_json` prints it. That output is valid CEL: `\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t` and `\u00XX` are all CEL escapes (langdef, String literals). It also matches today's quoted literals.
- **Integers** are decimal with no leading zeros and no `+`.
- **Floats** print as Rust's `{:?}` does: shortest round-trip, `1.0` for integral values, and an exponent without `+` outside Rust's plain range (`1e21`, `1.5e-7`, where JavaScript writes `1e+21`). `-0.0` is written `0.0`: the two are equal under every comparison here. The corpus pins `1e21`, `1e-7`, `-0.0` and `0.1` so a non-Rust printer can't drift.
- **Timestamps** print as `timestamp("YYYY-MM-DDTHH:MM:SS[.ffffff]Z")`, in UTC, with the fraction trimmed of trailing zeros and omitted when zero.
- **Durations** print as `duration("<seconds>[.ffffff]s")`, the protobuf JSON form, trimmed the same way.
- **Field segments** are bare when they are a `SELECTOR` and not a reserved word, and backticked otherwise (`` `latest-claim` ``). A map key goes in `["…"]` as a string.
- **`&&` and `||` are n-ary in the AST.** `a && (b && c)` parses to `And[a, b, c]` and prints `a && b && c`. An `||` operand of `&&` is parenthesised. The operand of `!` is parenthesised unless it is a field, `has(…)`, an `under` call or already parenthesised.
- **No reordering.** Conjuncts and list elements keep their written order. Canonical text fixes the spelling, not a normal form of the logic.

The strict parser refuses anything else. The lenient parser accepts any CEL spelling of the same AST: whitespace, single quotes, redundant parentheses, other escapes, `timestamp` with an offset, and `//` comments.

### 4.8 Wire form

- **Text is the wire and the storage form,** as today:
  - the `where` query parameter;
  - verb `when` in the presentation YAML and the baked CBOR (#4182 ruling 10);
  - stored conditions;
  - Layout documents.

  In CBOR (ADR 0063) a condition is a text string. The profile needs nothing new.
- **No JSON or CBOR AST is defined now.**
  - A second spelling of the same condition is what ADR 0066 removed.
  - The clients that would want an AST (C and TS) get the parser from the Rust crate through binnacle's existing C API and wasm build (§6).
  - If an AST interchange is ever needed, CEL's protobuf `cel.expr.ParsedExpr` (`proto/cel/expr/syntax.proto`) already exists, and the subset maps onto it one-to-one, with operators as calls such as `_==_` and `_&&_`.

### 4.9 Translation to SQLite

The translation is a direct mapping over the subset. Literals and bound variables become parameters, **already converted to the column's storage type**, so SQLite's affinity never applies:
- timestamps and durations become integer microseconds;
- bools become 0/1;
- enums become the stored code.

| Condition | SQL |
|---|---|
| `f == v`, `f != v`, `f < v` … | `f = ?`, `f <> ?`, `f < ?` … |
| `f in [a, b]` | `f IN (?, ?)` (never empty) |
| `has(f)` | `f IS NOT NULL` |
| bare bool `f` | `f = 1` |
| `!e`, `a && b`, `a \|\| b` | `NOT (e)`, `(a) AND (b)`, `(a) OR (b)` |
| `true`, `false` | `1`, `0` |
| `f.under(P)`, `f` a text column | `f = ?p OR (f >= ?p \|\| '/' AND f < ?p \|\| '0')` |
| `f.under(P)`, `f` an interned address column | `f IN (SELECT id FROM strings WHERE domain = 'ad' AND (s = ?p OR (s >= ?p \|\| '/' AND s < ?p \|\| '0')))` |
| `"k" in labels`, `labels["k"] == v` | the store's label index (resource kinds today, `sqlite.rs:1139-1205`) |

**Why the translation is exact.** U is SQL's NULL: an absent value is a NULL cell, and every comparison and connective above gives NULL exactly where §4.5 gives U. A top-level NULL drops the row, which is SQLite `WHERE` (lang_expr §14) and §4.5's filter column. `NOT (NULL)` is NULL, matching `!U = U`.

The `under` range is sound under `BINARY` collation, because `0` is the byte after `/` and canonical values never contain a bare `/` (ADR 0066 §4, `descendant_bounds`). The interned form scans the `UNIQUE(domain, s)` index of `strings` (`crates/flotilla-schema-sqlite/src/shared.rs:46-51`), rather than comparing text from correlated subqueries as `table_read.rs:63-83` does today.

**Identifiers come only from the schema.** The translator never writes a field's path text into SQL. Each field resolves through the store's `ColumnMap` to a column expression the store wrote, and literals are always bound parameters. A backticked segment may hold `.`, `-`, `/` and spaces, and `where` is client input, so this is an invariant of `rigging-condition::sql`, with corpus cases for hostile names (`` `a" OR 1=1 --` `` and similar) that must give `unknown_column` or a parameter, never SQL. Flattened and child-table columns are reachable only where the `ColumnMap` gives an expression for them.

**Query plans are pinned.** The SQL corpus runs `EXPLAIN QUERY PLAN` on the `under` forms and on `in`. The interned form must use the `strings` `UNIQUE(domain, s)` index, not scan the table per row.

Which fields a table may filter on is now declared by the schema. A field the store cannot push down (a `cbor` column, or a map with no index) is simply not declared in the `where` schema, so `whereOperators` becomes redundant. Resource-kind tables, which today push down only `==`, can evaluate the expression in memory over the decoded rows before paging, until the ADR 0064 cut-over retires them.

## 5. Migrating today's spelling

Wire formats change freely (ADR 0047 §1). Stored data stays decodable for one generation (ADR 0047 §2), and a shim retires only after a re-encode step has run on every host (ADR 0066 §6).

### Which spelling a text is in

The legacy expression part always starts with `.`, as in `.state == merged`. The new canonical text never does (§4.2). So a decoder reads the subject (an address or `$var`, with no change to ADR 0066's subject grammar) and looks at the next character:
- `.` means a legacy comparison, parsed by the existing `condition.rs` code, which moves to a `legacy` module;
- anything else means the new language.

The rule is unambiguous without lookups.

### Converting a legacy condition

The conversion needs the field's type from the schema (§4.3):
- **The path:** segments that are not `SELECTOR`s are backticked, and a `labels.<k>` path becomes a map or list form.
- **The literal** is typed from the field:
  - `string` and `enum` are quoted;
  - `int` and `bool` are as written;
  - a `timestamp` becomes `timestamp("…")`.

| Legacy (stored today) | New canonical |
|---|---|
| `forge:github/repo:flotilla-org%2Fflotilla/cr:1363 .state == merged` | `forge:github/repo:flotilla-org%2Fflotilla/cr:1363 state == "merged"` |
| `convoy:demo .latest-claim.disposition == "changes pushed"` | ``convoy:demo `latest-claim`.disposition == "changes pushed"`` |
| `$issue .labels."kind:bug" == true` | `$issue "kind:bug" in labels` (see labels below) |
| `$issue .updated-at > 2026-10-01T00:00:00Z` | ``$issue `updated-at` > timestamp("2026-10-01T00:00:00Z")`` |
| `$cr.state == merged` (old-store workflow template) | `$cr state == "merged"` |
| `$cr .head-sha == revision` (promise kind) | ``$cr `head-sha` == revision`` (a declared bound variable) |

**Issue labels.** Today `issue .labels.<k>` reads `"true"` when the observed label set holds `k` and `"false"` when it does not. It reads Unknown when the label set is unobserved or stale (`leaf.rs:331-334`). Admission allows only `true` and `false` as literals (`leaf.rs:123-125`). So there are four cases, and each keeps its U behaviour:

| Legacy | New | Label set observed, `k` held | observed, `k` not held | unobserved or stale |
|---|---|---|---|---|
| `.labels.k == true` | `"k" in labels` | T | F | U |
| `.labels.k != false` | `"k" in labels` | T | F | U |
| `.labels.k == false` | `!("k" in labels)` | F | T | U |
| `.labels.k != true` | `!("k" in labels)` | F | T | U |

`labels` is a `list(string)` field that is unknown when the set is unobserved, so `!U = U` gives today's Unknown in the last column.

**A legacy condition that does not type-check.** The old admission never checked some literals, such as an enum value outside the set (`leaf.rs:117-128`), so such a condition can be stored today, and it is never true. It decodes to a **decode-only AST node, `Legacy(text)`**. The node keeps the legacy canonical text exactly as stored, evaluates to U for every row, and prints as that same legacy text. Its spelling starts with the subject and then `.`, so the next decode recognises it again (above). Its interned key, gate key and frozen bytes stay byte-identical, and nothing is re-keyed for it. The checker refuses `Legacy` in new input, so no new condition can take this form. The re-encode step leaves these records alone and reports how many there are. The legacy parser can retire only when that count is zero on every host: these conditions sit on messages, so they go when the messages are archived (#3939). It is never a decode failure, because ADR 0047 forbids one.

### Per stored form

- **Interned engine conditions (message outcome and delivery condition).** These decode through the legacy parser and convert. The re-encode step of ADR 0066 §6 rewrites the interned text, so its marker retires the shim.
  - **Risk:** the message gate is keyed by the condition's text (`message.rs:734-736`), so the same step must re-key existing gate rows. Otherwise a held message reads its gate as unobserved after the roll.
- **`Frozen<PromiseKinds>`.** The objects are content-addressed, so rewriting one changes its hash and every reference to it. **Decode-time conversion only.** The shim retires once no live convoy holds a legacy-frozen object. Convoys are finite, and the pre-roll live-store gate (#2167) can check this.
- **Old-store workflow templates, frozen workflow snapshots and Leaf JSON.** These already convert through `Conversion::condition` (`crates/flotilla-core/src/primitives/convert.rs:186-215`) in the ADR 0064 cut-over importer. That conversion prints the new language instead of the 0066 grammar, so they skip the intermediate spelling.
- **Input.** `wait --for` and CLI conditions accept both spellings through the lenient parser for one generation. The `where` parameter switches outright, because it is wire, and outstanding cursors that embed the old filter degrade to `resync` (`tables.rs:329-339`).
- **Enum spellings.** The three spellings (§1) must become one per field before conversion, because the literal is checked against the field's value set. The language needs a single spelling per field: whatever the rows wire carries in that column, so a client compares cells to literals with no mapping. This note recommends the readable name (`Landed`, `merged`) on the wire, with the short code used only in storage and SQL translation. That choice belongs to the descriptor work, not the language.

## 6. binnacle's View Spec filters on the same subset

binnacle can adopt the language in three steps. Each works on its own.

1. **Now, with no binnacle change.** A flotilla Table Source declares `native: true`, and its Native Conditions are expressions in this language, passed through as `where` (binnacle `CONTEXT.md`, "Native Condition"). The source turns its OneOf and Compare conditions into the language for the server: OneOf becomes `in`, a Compare becomes a relation, and `not` becomes `!`.
2. **binnacle's in-memory sources evaluate the language** through `rigging-condition`. binnacle-core is Rust; its web adapter is a wasm build of it, and its C API (`binnacle-ffi`) wraps it. So the C and TS clients get the Rust evaluator, and **no separate TS or C implementation is needed**. A native TS evaluator is optional, built only if the wasm size matters, and must then pass the same corpus.
3. **The Layout document stores expressions.** `FilterSpec` gains a list of expressions, ANDed with the quick filter. `ConditionTree` is `#[non_exhaustive]` for this. The structured kinds become editing shapes over it: a UI that sees a single-field `in` or relation shows a checklist or a range, and shows any other expression as text. Layout goes to version 3, and version 2 conditions convert on load.

How binnacle's current kinds map, and where the meaning changes:

| binnacle today | Same subset | Semantic change |
|---|---|---|
| `OneOf([a, b])` | `f in ["a", "b"]` | none |
| `OneOf([a, Blank])` | `!has(f) \|\| f in ["a"]` | none |
| `Compare(Less(x))`, etc. | `f < x` | none (blank and NaN never pass) |
| `Compare(Between(a, b))` | `f >= a && f <= b` | none |
| `Compare(…)` on a date | `f < timestamp("…")` | date-only means midnight UTC, as today |
| `not` on any of these | `!(…)` | **changes.** binnacle's `not` keeps blanks today, but `!U = U` drops them. A literal translation of today's behaviour is `!has(f) \|\| !(…)`. |
| `Text(TextQuery)` (contains, ignore case, regex) and the quick filter | not in the subset | none. These stay a binnacle search feature. |
| `Native(String)` | the expression itself | none |

The `not` change is the only one users can see. This note recommends the Kleene meaning ("not less than 5" excludes rows with no value), with the version 2 → 3 conversion writing the explicit `!has(f) ||` form so saved layouts keep their results.

## 7. Layering against an RDF or SPARQL direction

The expression language sits at **one layer: a predicate over one solution**, meaning a set of named, typed values. That is exactly the layer SPARQL calls `FILTER` (§17): its expressions are evaluated "with respect to a solution mapping", in a sub-grammar (§19.8 [110]…) separate from graph patterns.

What *produces* the solutions is a different layer:
- today, a table row, or a leaf subject's observed fields;
- later, perhaps, a basic graph pattern over addresses and their links.

Choosing this language commits flotilla to neither.

The subset was chosen as the intersection, so a later graph layer can use it as its FILTER with a **mapping of terms, not a change of meaning**:

| Here | SPARQL / RDF |
|---|---|
| field | variable `?f` |
| absent or unknown, U | unbound, error (SPARQL has no separate unknown; `BOUND` is never an error, so `has` on an unknown field has no exact SPARQL form) |
| `has(f)` | `BOUND(?f)` |
| `&&` `\|\|` `!` | `&&` `\|\|` `!` (the same table, SQ §17.2) |
| top-level U drops the row | FILTER drops the solution (SQ §17) |
| `f in [a, b]` | `?f IN (a, b)` |
| `int`, `number`, `string`, `bool` | `xsd:integer`, `xsd:double`, `xsd:string`, `xsd:boolean` |
| `timestamp` | `xsd:dateTime` |
| `duration` | `xsd:dayTimeDuration` (comparison is an engine extension, SQ §17.3.1) |
| `address` | IRI, through the link form `https://flotilla.work/<fleet>/<address>` (#4182 ruling 5) |
| `enum` | string literal (or an IRI per value, if a vocabulary is published) |
| `f.under(P)` | `?f = <P> \|\| STRSTARTS(STR(?f), CONCAT(STR(<P>), "/"))`, or an IRI-named extension function |

Two SPARQL differences cannot arise in the subset:
- SPARQL's `=` on incompatible literals is an error rather than false (SQ §17.4.1.7). Here the checker refuses an ill-typed comparison before evaluation.
- SPARQL promotes between numeric types. The subset's `int` and `number` already compare on the number line.

Typed fields stay typed. Each field's type comes from its schema, so any RDF view is a **projection** of the known types, never a replacement for them.

What the layering means in practice:
- `rigging-condition` must not know where its rows come from. Its `Row` trait is the solution mapping.
- Field paths are names in that mapping. A dotted path is a name with structure, not a graph traversal. Following a link to another subject belongs to the solution layer, through related tables today (#4108) and graph patterns later, never to the expression.

## 8. The crate shape in rigging

One crate, `rigging-condition`. It has no flotilla dependencies (ADR 0068 §3) and no I/O.

| Module | Contents |
|---|---|
| `syntax` | Lexer, strict and lenient parsers, canonical printer, limits |
| `ast` | `Condition` (n-ary `And`/`Or`, `Not`, `Has`, `Compare`, `In`, `Member`, `Under`, `Field`, `Bool`), `Field`, `Literal`, `Variable` |
| `check` | `Schema` (field → `Type`, nullable; declared variables), the type rules of §4.3, an optional literal-validation hook for `address` and enum sets, and `Checked<Condition>` as proof |
| `eval` | `Row` trait (`get(&Field) -> Option<Value>`), `Truth { T, F, U }`, `bind(variables)`, `evaluate`, and the first non-T conjunct as the reason |
| `sql` | Translation to SQLite text plus parameters, through a `ColumnMap` the store supplies (column expression, storage type, interned domain). It produces strings only, with no `rusqlite` dependency. |
| `corpus/` | The conformance corpus (below) |

Serde for `Condition` is the canonical text, matching `text_serde!` in `condition.rs` today. There is no CBOR-specific code.

**The conformance corpus.** It is a JSON file of cases, so the Rust tests, binnacle's wasm vitest and any later native TS evaluator all read it. Each case holds:
- a schema;
- the input text, and whether it is strict or lenient;
- an expected check error, or the canonical print;
- rows, each with its expected T, F or U.

Cases marked for SQL run against an in-memory SQLite, and must keep exactly the rows that evaluate to T. Two sources feed it:
- **hand-written cases** for every rule in §4: absent values under each operator, Kleene tables, code-point ordering outside the BMP, `under` on segment boundaries (`demo` against `demo2`, percent-encoded `/`), timestamp precision, empty `in`, limits and canonical spelling, unknown versus absent under `has` and `!has`, fields behind a missing sub-structure, hostile identifiers, float printing, and query plans;
- **a generated slice of cel-spec** `tests/simple/testdata` (`logic`, `comparisons`, `fields`, `lists`, `timestamps`), restricted to expressions inside the subset.

A dev-only differential test also runs every corpus expression through the `cel` crate (0.15.0, which runs cel-spec v0.25.3), mapping its error to U. That checks the compatibility promise of §3.

**Size.** The subset is small enough to implement and own:
- one recursive-descent parser over about 15 productions;
- a printer;
- a checker over 10 types;
- an evaluator over 9 node kinds.

**Not owning** it would mean depending on a general CEL runtime at run time in every client. The C client would then need cel-cpp, which is Bazel- and protobuf-heavy, and there is no pure-C CEL. The runtime would also not give canonical printing, the strict checker or three-valued results.

### What stays in flotilla

- **Field resolution:**
  - schemas from table descriptors (`descriptor.rs:220-241`), including the `address` type that #4108 adds as a role;
  - the leaf vocabulary (`leaf.rs:8-24`) and the promise vocabulary (`kinds.rs:59-115`) become schemas;
  - `Row` implementations over table rows and leaf subjects, keyed by full declared path. An optional sub-structure that is missing (no latest claim) makes its fields absent, or unknown for an unobserved subject, and never a missing intermediate. Stale and unobserved mean unknown.
- **Address literals:** validation as canonical text, and lenient resolution through `ForgeLookup`.
- **The subject layer:**
  - `<address> <expression>`;
  - `$var` templates with `bind`;
  - multi-subject `wait`.
- **Legacy decoders and conversion** (§5), and their retirement markers.
- **Per-consumer handling of U** (§4.5) and the store's `ColumnMap` for SQL (interned columns, codes).

## Open points for the owner

1. **The `not` change in binnacle filters** (§6): adopt Kleene and convert saved layouts, or keep two-valued `not` as binnacle-only sugar.
2. **Enum spelling on the wire** (§5): readable names, with codes only in storage, is this note's recommendation, but it belongs to the descriptor work.
3. **`under` moves to rigging** (§4.6), against the brief's placement.
4. **Text search** (`contains`, case folding, regex) stays out of the subset for now.
5. **The engine gate's Unknown-as-false collapse** (§4.5, #4204) predates this note, but the migration touches the same gate keys.

## Sources

Retrieved 2026-10-31.

**CEL**
- **cel-spec** at https://github.com/google/cel-spec, commit `21ec6f4e52e8d694687ff9ed7408d3eccbb955d7` (latest release v0.25.3):
  - `doc/langdef.md`: Syntax (EBNF, precedence, limits), String literals and escapes, Evaluation, Logical Operators, Field Selection (`has`), Equality, Ordering, Types and Conversions (`timestamp`, `duration`), Overflow, Extension Functions, Receiver Call Style, Performance;
  - `tests/simple/testdata/*.textproto` (`fields.textproto` section `quoted_map_fields`, `map_no_such_key`, `map_no_such_key_or_true`, `…_and_false`; `logic.textproto`);
  - `proto/cel/expr/syntax.proto` and `checked.proto`.
- **cel-rust** (crate `cel` 0.15.0) at https://github.com/cel-rust/cel-rust, commit `b2871f486f3418ca7f261beafafaf39a7d3d712e`: `cel/src/parser/gen/CEL.g4` (`ESC_IDENTIFIER`), `conformance/Cargo.toml`, `conformance/src/bin/ignored.txt`.
- **cel-es** (`@bufbuild/cel` 0.6.1) at https://github.com/bufbuild/cel-es, commit `8f0c64a73e9f5f57b25c59d66c0f952233caea85`: `packages/cel/src/conformance.test.ts`.
- **cel-js** (`@marcbachmann/cel-js` 8.0.0) at https://github.com/marcbachmann/cel-js, commit `86cd97216a1cbb522eda81aaa0221c245d5913d7`.
- **cel-go** README at https://github.com/cel-expr/cel-go, commit `3a0121bae2bf334df0eeda2d1136bb212c19d4aa` (Partial State).
- **Kubernetes, CEL:** https://kubernetes.io/docs/reference/using-api/cel/ (type checking, resource constraints).
- **SPANDigital/cel2sql:** https://github.com/SPANDigital/cel2sql (README, dialects).

**SQL**
- **SQLite:** https://www.sqlite.org/lang_expr.html (§2 operators and NULL, §5 LIKE, §8 IN, §14 boolean expressions), https://www.sqlite.org/nulls.html, https://www.sqlite.org/datatype3.html (§2 storage classes, §4.1–4.2 comparison and affinity, §7 collation).

**SPARQL**
- **SPARQL 1.1 Query Language**, W3C Recommendation 21 March 2013: https://www.w3.org/TR/sparql11-query/ (§17, §17.2, §17.2.2, §17.3, §17.3.1, §17.4.1.7, §17.4.1.9, §17.4.3.6, §17.6, §18, §19.8).
- **SPARQL 1.2 Query Language**, Working Draft 25 October 2026: https://www.w3.org/TR/2026/WD-sparql12-query-20261025/ (§17.2.3).
- **w3c/rdf-tests** at commit `d01b141c65ddf8e5b615256f264111118bdb2134` (`sparql/`).
- **oxigraph** v0.5.13 at commit `7dc1266c55a0bc5d73e8d1f8cf2723d1490ce149`: `lib/spareval/src/expression.rs`.
- **rasqal** at https://github.com/dajobe/rasqal, commit `9e0914fc833354d8f8a562b80e05cc1b73acd345` (NEWS).

**Other candidates**
- **AIP-160** at https://google.aip.dev/160, source `aip/general/0160.md` at commit `2ccc61a4e09882bdd63aea1a42dceebc37bf3bd5`; EBNF at https://google.aip.dev/assets/misc/ebnf-filtering.txt. **einride/aip-go** v0.86.3 at commit `b8daf7c28ca22f7b2ec7afbd0fd28d33dc3817ea` (`filtering/expr.go`, `functions.go`).
- **OData v4.01** Part 2 URL Conventions, OASIS Standard 23 April 2020 (§5.1, §5.1.1.1, §5.1.1.17, §5.1.2), and the ABNF construction rules with `odata-abnf-testcases.xml`: https://docs.oasis-open.org/odata/odata/v4.01/.
- **JSONLogic:** https://jsonlogic.com, https://jsonlogic.com/tests.json, and json-logic-js v2.0.5 at commit `c5c73601c90b11e98f6846609bac4dec203d1c18` (`logic.js`).
- **FEEL:** OMG DMN 1.5, formal/24-01-01, chapter 10 (§10.3.1.2, §10.3.2.2, §10.3.2.4 Tables 50–51, Table 49), https://www.omg.org/spec/DMN/1.5/PDF. DMN TCK at https://github.com/dmn-tck/tck.
- **exprtk** at https://github.com/ArashPartow/exprtk, commit `1e4a80b5ec9b4832ed59c6faa65f625a01b18ef0` (`readme.txt` §01, §08, §09, §15, §24, §30).
- **Perspective** v5.6.1 at commit `444865accd46bcfdeb552960ce4f63fe8286de42`: `docs/md/explanation/view/config/expressions.md`, `selection_and_ordering.md`, and `rust/perspective-client/src/rust/virtual_server/generic_sql_model/table_make_view.rs`.
- **Kubernetes selectors:** https://kubernetes.io/docs/concepts/overview/working-with-objects/labels/ and https://kubernetes.io/docs/concepts/overview/working-with-objects/field-selectors/.
- **ECMA-262**, the `IsLessThan` abstract operation (string comparison by UTF-16 code units): https://tc39.es/ecma262/.

**flotilla and binnacle**
- **flotilla** at commit `34d7ad85afc8c9014922e669bab868a78c3c395e`. Files are cited inline.
- **flotilla ADRs** 0047, 0063, 0066 and 0068, read from `34d7ad85`'s history at `111d008c^`, since #4067 removed `docs/adr/` from the tree.
- **flotilla issues:** #4105, #4108, #4182 (rulings 1–10), #4195.
- **binnacle** at commit `074be4cabe73ed6613d629a197f2cb826493bc3e`: `crates/binnacle-core/src/view.rs`, `web/src/source.ts`, `CONTEXT.md`. Also binnacle#297.
