# Verbs on typed things: prior art

**Date:** 2026-10-29
**Status:** complete (research; no code changes). The recommendation and its YAML examples are illustrative input to the verbs-as-data design, not a spec; the `verbs` table it proposes still goes through schema review before it is built.
**For:** flotilla#4107, part of mission flotilla#4105 (decision 3: row actions are verbs on subject kinds)

## Question

What did earlier systems that attach actions to typed things on screen find worth having, and what turned out to be too much? The answer shapes flotilla's verb document. Verbs are row actions in described tables, and generic clients render them: Wheelhouse native, web, TUI and the CLI. The same verbs are also served to agents as MCP tools.

## Answer

Every system that lasted keeps the same small core:
- a verb is keyed by the type of the thing it acts on;
- it has a name, a label and typed parameters;
- one parameter can be filled by pointing at another typed thing on screen;
- there is a cheap test for whether it applies;
- one verb per type is the default.

CLIM had all of this in 1991. The parts later called heavy were all added around that core:
- a presentation-type lattice with parameter massaging;
- per-view `accept`/`present` methods;
- a translator matrix keyed by gesture, with priorities;
- arbitrary tester code run on every pointer motion.

Embark gets the core with almost none of that: a flat map from type symbol to keymap, actions that are ordinary commands, and a short confirmation list. It also removed a feature, `embark-allow-edit-actions`, to stay small.

The hypermedia formats (Siren, HAL-FORMS) settled the wire vocabulary for typed fields: name, type, label, required, default and options. Their applicability rule is "the server includes the action or doesn't", decided per response. That costs a server evaluation and payload for every row, and it gives a client nothing to grey out.

MCP's tool shape (name, description, `inputSchema`, hint annotations) is a straight projection of a verb that takes no pointing. AG-UI's frontend tools point the other way: the client defines the tool and the agent calls it. flotilla has no need for that.

**Recommendation in one line:** verbs are rows in a `verbs` table keyed by subject kind, fetched once per kind. Each verb has:
- an applicability condition over row columns, in the existing `where` grammar, which the client evaluates and the server re-checks;
- typed parameters, where one type is "pick a row of kind K";
- a `command` body (a daemon command plus an argument binding) or an `open` body (a relation from a small shared registry plus the subject identity);
- a confirm text, an effect class (read-only, changes state, destructive) and a `picks` arity.

Leave out inheritance, gestures, per-row actions, predicate code and per-verb URLs.

## Sources

- **CLIM II specification.** Online at http://bauhh.dyndns.org:8000/clim-spec/ (sections cited as `23-7.html` etc.). A LaTeX copy is in McCLIM `Documentation/Specification/*.tex`; McCLIM notes it carries "a few modifications".
- **McCLIM** at https://github.com/McCLIM/McCLIM, commit `c5bc6cb7f03ce0d77211403642caa1e3e8f963d0`. Its primary home is codeberg.org/McCLIM/McCLIM.
- **Franz CLIM-TOS** (descended from the Symbolics code) at https://github.com/franzinc/clim2, commit `6af41f1a6457ddcafbbde47939b432fe78453fdb`.
- **Franz CLIM 2.2 User Guide:** https://franz.com/support/documentation/10.1/doc/clim-ug.pdf
- **Symbolics Genera 8, *Programming the User Interface*** (Dynamic Windows): http://www.bitsavers.org/pdf/symbolics/software/genera_8/Programming_the_User_Interface.pdf. Cited as doc page / PDF page.
- **Strandh & Moore, "A Free Implementation of CLIM"** (2002): https://mcclim.common-lisp.dev/static/documents/clim-paper.pdf
- **Moore, "An Implementation of CLIM Presentation Types"**, J.UCS 14(20), 2008: https://www.jucs.org/jucs_14_20/an_implementation_of_clim/jucs_14_20_3358_3369_moore.pdf
- **Embark** at https://github.com/oantolin/embark, commit `87e53827cf6659dcc4ac4e54be9af34aeca44f6e` (version 1.2): `README.org`, `embark.el`, `NEWS.org`.
- **VisiData** at https://github.com/saulpw/visidata, commit `5f67ff8af28c59a8f7429e396601606f2ac8dd47`, and the man page at https://www.visidata.org/man/
- **Siren** at https://github.com/kevinswiber/siren, commit `c29a878` (README is the spec).
- **HAL-FORMS** at https://rwcbook.github.io/hal-forms/, source https://github.com/rwcbook/hal-forms `index.asciidoc`, commit `4b827d1`.
- **AG-UI** at https://docs.ag-ui.com, source https://github.com/ag-ui-protocol/ag-ui, commit `7e7c110` (spec 1.0).
- **MCP specification revision 2026-07-28** at https://modelcontextprotocol.io/specification, source https://github.com/modelcontextprotocol/modelcontextprotocol, commit `9850cae`.
- **Plan 9 plumb(6):** https://9p.io/magic/man2html/6/plumb
- **Apple App Intents:** https://developer.apple.com/documentation/appintents
- **Apple Services Implementation Guide** (archived 2016).
- **DMTF Redfish-Publications** at commit `fa6d23d`: `csdl/RedfishExtensions_v1.xml`, `json-schema/ActionInfo.v1_5_0.json`.
- **Client asks:** the Actions and edits section of https://github.com/flotilla-org/wheelhouse/issues/505, and the binnacle viewer survey (https://github.com/flotilla-org/binnacle/tree/research/described-tables/docs/research, file 11, the https://github.com/flotilla-org/binnacle/issues/289 section). The binnacle file was not reachable from here, so the list relayed by the operator was used.
- **flotilla** at commit `14875d157`.

---

## CLIM (and Symbolics Dynamic Windows before it)

### What it has

**Presentations.** Output is recorded as presentations: an object paired with a presentation type.

**Commands with typed arguments.**
- A command is a name plus arguments. Each argument is `(var ptype &key default prompt documentation when gesture …)` (`commands.tex:105-145`; spec 27.1).
- Arguments with no default get `*unsupplied-argument-marker*`, which makes the command partial. A partial command is completed by prompting:
  - at the end of the command line when possible, otherwise through an `accepting-values` dialog (spec 27.6, `commands.tex:957-980`);
  - McCLIM loops that dialog until no arguments are unsupplied (`Core/clim-core/commands/processor.lisp:105-175`).

**Pick-to-fill.** While an argument of type X is being read, every visible presentation of type X or a subtype becomes sensitive. It highlights under the pointer and a click supplies it.
- The spec puts it as "Only the presentations that 'match' the input context will be 'sensitive'" (spec 23.5, `presentation-types.tex:1060-1071`).
- The McCLIM manual's month and day example shows parameters such as `(integer 1 31)` narrowing what is clickable (`Documentation/Manual/user-manual.texi:572-640`).

**Presentation-to-command translators.**
- `define-presentation-to-command-translator` maps a from-type plus a gesture to a command. Its body returns the argument list (`presentation-types.tex:1539-1570`).
- Declaring `:gesture` on a command argument generates such a translator, and "the other arguments will be filled in with their default values" (`commands.tex:133-145`).

**Command tables.**
- They hold commands, names, keystrokes, menus and translators, and they inherit "by union with shadowing" (spec 27.2, `commands.tex:164-196`).
- Enablement is per frame, through `(setf command-enabled)`. Its notify hooks exist for "graying out" (spec 28.4, `frames.tex:872-896`).

**Default verb and menu.** The `:select` gesture (left click) runs the default translator, and `:menu` (right click) lists every applicable one (spec 22.3.1; `Core/clim-core/builtin-commands.lisp:83-135`).

### Worth having

- **Arguments typed by the kind of thing on screen, filled by pointing.** This is flotilla's "parameter of subject kind, filled by picking a row".
- **Partial commands.** Pointing supplies some arguments, defaults fill others, and the client prompts only for the rest in one generic dialog. No per-command UI code is needed.
- **A default action plus a menu of the rest.**
- **Disabled shown as greyed, not removed.**

### Too much

**The presentation-type lattice.**
- Types "form a type lattice, an extension of the Common Lisp CLOS type lattice" (spec 23.1). The spec carries an unresolved reviewer note right there: "Why are they in a lattice?… What exactly gets inherited?" (`presentation-types.tex:48-51`).
- Parameters must be massaged from subtype to supertype during method combination (`presentation-types.tex:222-225, 383-411`), with another open note on how (`:390-391`).
- Moore calls this "the major remaining complexity", handled by "somewhat complicated macro programming" (J.UCS 2008, pp. 3367-3368).
- McCLIM's maintainer notes record that:
  - `or`, `and` and `completion` break the hierarchy (`Documentation/Notes/presentation-types.org:96-153, 458-461`);
  - plain-class inheritance and `and` multiple inheritance "doesn't work" (`:393-419`);
  - the core authors disagree on whether `presentation-typep` is strict (`:273-324`).
- A 2020 code comment is still mid-repair: "we work hard on improving the presentation type inheritance" (`Core/clim-core/presentations/presentation-types.lisp:49-55`).

**`accept` and `present` methods per type and per view.**
- Methods specialise on a view (textual, dialog, gadget and others; spec 23.6), and for a given view "the present and accept methods must be duals" (Franz UG 8.6.9, p. 137).
- Gadget views exist only for a fixed table of type and view pairs (Franz UG 8.6.13, pp. 147-148). McCLIM falls back to textual for the rest (`Core/clim-core/dialogs/dialog-views.lisp:88-108`).
- Rendering is code per type and per client kind. That is exactly what flotilla wants gone.

**The translator matrix.**
- Matching runs six ordered steps over from-type, to-type, gesture, typep, tester, and body re-check (spec 23.7.4, `presentation-types.tex:1934-1960`).
- Ties are ordered by a two-part priority, then type specificity, then table locality (`:1751-1765`).
- Franz calls the priority algorithm "complicated" (UG p. 155). The McCLIM code comments "That's pretty wacked..." (`Core/clim-core/presentations/translators.lisp:318-321`).
- When the body runs was never pinned down. The spec has an open note to "Be very clear and precise about when the translator body gets run" (`presentation-types.tex:1776-1777`).

**Tester functions run during pointer motion.**
- Testers are arbitrary code, run on every motion event (McCLIM `Core/clim-core/frames/frames.lisp:855-896`).
- The spec asks implementations to cache lookups "whenever the user mouses the pointer around" (`presentation-types.tex:1664-1668`).
  - McCLIM's cache is flushed by any translator definition (`translators.lisp:77-110, 289-358`).
  - The cache was not thread-safe: McCLIM issue #1310.
- Dynamic Windows' own guide, "Some Efficiency Caveats for Mouse Handlers", warns:
  - "the more there are, the slower everything becomes";
  - "Keep :tester forms fast";
  - "Translators run when you move the mouse; commands do not run until you ask for them".

  (Genera 8 doc pp. 1845-1847 / PDF pp. 237-239.)
- Enablement checked in one place was bypassed in others: disabled commands could still run from keystrokes or old presentations (McCLIM issue #1144; `processor.lisp:417-434`).

**Lesson for flotilla.** Keep "typed arguments, pick-to-fill, default plus menu, greyed when not applicable". Make applicability a declarative condition the client can evaluate without running verb code. Have one flat kind namespace with no inheritance. Render parameters from their declared types, not from per-type methods.

## Emacs Embark

### What it has

**Types and keymaps.**
- `embark-keymap-alist` maps a type symbol (`file`, `buffer`, `url`, …; `t` is the fallback) to one or more keymaps of actions (`embark.el:130-164`).
- Types are bare symbols looked up with `alist-get` (`embark.el:1104-1107`).
- Sharing between types is only keymap `:parent`, for example `command-map` → `function-map` → `symbol-map` (`embark.el:4591-4640`).

**Target finders and transformers.**
- Target finders are an ordered hook list returning `(TYPE . TARGET)` (`embark.el:166-203`).
- Transformers turn one type into another, for example `project-file` → `file` (`embark.el:205-217`).
- Types usually come from completion `category` metadata (README §Marginalia, L109-117).

**Actions are ordinary commands.**
- Embark "calls the command with `call-interactively`" and injects the target by inserting it into the first minibuffer prompt, "simulating user input" (README §How does Embark call the actions?, L974-995; `embark.el:2107-2201`).
- Later prompts behave normally, so further arguments are asked as usual (README L986-1011).

**Default action.**
- `embark-dwim` runs the default action with no menu (`embark.el:2606-2643`).
- The default is chosen in this order: overrides, then the command that opened the minibuffer, then `RET` in the type's map (`embark.el:2309-2326`; README §The default action, L127-143).

**Confirmation.**
- `embark--confirm`, a `y-or-n-p`, is attached through `embark-pre-action-hooks` to a short list of "hard to undo" commands: `delete-file`, `kill-buffer`, `package-delete` and a few others (`embark.el:428-435`).
- `embark-quit-after-action` decides whether the menu closes (`embark.el:289-309`).

**Multiple targets.**
- `embark-act-all` runs the same action from the same keymap over every candidate or selection. It suppresses per-item confirmation and asks once: "Run %s on %d %ss?" (`embark.el:2483-2558`, especially L2514-2528).
- `embark-multitarget-actions` names the actions called once with the whole list instead (`embark.el:529-538`).

**Collect and export.**
- `embark-collect` and `embark-export` turn a candidate set into a buffer of typed rows that still carry their type for acting (`embark.el:3065-3145`, `2732-2753`).
- The action list appears after a delay, "which-key-like", so "Embark stays out of the way" (`embark.el:1829-1846`).

### Worth having

- **A flat type → actions map.** Configuring it "requires no programming" (README L77-89).
- **Actions that are just existing commands,** with the target filled in as the first argument. A verb is a reference to an existing daemon command plus an argument binding, not new code.
- **One default action per type,** run without a menu.
- **One action set, two arities.** `embark-act` and `embark-act-all` use the same keymap and confirm once for the batch. A short list of actions takes the whole set in one call.
- **Confirmation as a property of the action,** not of the call site.

### Too much, or removed

- **`embark-allow-edit-actions` was removed** "to simplify Embark", in favour of the general injection hooks (README L666-682).
- **Category helpers moved out** to Marginalia (README L1108-1112).
- **Collect-only marking was replaced** by general selection (NEWS v0.22.1).
- **The which-key indicator was kept out of core** (README L480-484).
- **Embark's pre/post/around hook alists are extension points flotilla does not need.** They are an Emacs-local customisation layer (`embark.el:399-527`). In a wire format they would become client-side code per verb.

## VisiData (arity as separate commands)

- Every command is registered with a default key, a stable long name, an exec string and a help line: `addCommand(keystrokes, longname, execstr, helpstr)` (`visidata/settings.py:366-376`). Keys rebind separately with `bindkey` (`settings.py:404`).
- **One-row and selected-rows forms are separate commands.** `d` → `delete-row` and `gd` → `delete-selected` (`visidata/clipboard.py:207-208`). The man page lists them as "d gd delete current/selected row(s)". The `g` prefix "often refers to selected rowset" (`visidata/guide.py:30`).
- **Worth having:** stable names separate from keys; both arities visible in the menu.
- **Cost:** each `g` variant is written by hand per command and per sheet type (for example `Canvas` defines its own `gd`, `canvas.py:847-853`). That works when commands are code. As data it would double the verb file.

## Siren and HAL-FORMS

**Siren.**
- Actions are an array on each entity: "Actions show available behaviors an entity exposes" (README §Actions, L175).
- Each action has `name` (required, unique per entity), `class`, `method` (GET if omitted), `href` (required), `title` and `type` (README L179-203).
- Each field has `name`, `class`, `type` (HTML5 input types, default `text`), `value` and `title` ("Clients may use this as a label") (README L211-234).

**HAL-FORMS.**
- `_templates` describes "the available state transitions", keyed `default` when there is one (`index.asciidoc:93-95`).
- A template has `method` (required), `contentType`, `target` (falling back to `self`) and `title` (`:99-116`).
- Properties have `name`, `prompt`, `required`, `readOnly`, `regex`, `value`, `templated`, `type`, min/max/length/step, and `options`, which are either `inline` values or a `link` to fetch (`:126-343`).
- The spec is a "Working Draft" for "experimental and 'proof-of-concept' apps" (`:15`).
- Its suggested flow fetches form documents "either on-demand or in pre-fetch mode" (`:755-800`), one extra round trip per form.

**Applicability is per response.** Neither spec has a client-side applicability rule. An action is available if the server put it in this representation; that is a reading of both specs, neither says it outright.

**Worth having:**
- The field vocabulary: name, label, type, required, default value, inline options, and options by link.
- Enumerated options by link correspond to "pick a row from another table".

**Too much for a table of many rows:**
- Per-entity inline actions mean the server evaluates applicability and serialises the actions for every row on every change.
- A client that receives no action cannot tell "not applicable" from "not offered", so it cannot grey a verb out. Wheelhouse and binnacle both ask to grey verbs out.
- `method` and `href` per action tie the verb to a URL layout. One generic invoke route does the same job.

## AG-UI

**Frontend tools.**
- `RunAgentInput.tools` holds "The application's own tools — frontend tools"; "The application executes them, not the agent" (spec 1.0 `basic/run-input.mdx:88-99`).
- A `Tool` has `name`, `description` and a JSON Schema `parameters` that the protocol carries "opaquely" (`schema.json:1531`).
- A frontend tool call ends the run. The next run must carry a tool message answering it (`events/tool-calls.mdx:155-217`).

**Event stream.**
- `RUN_STARTED`/`FINISHED`/`ERROR`, `TOOL_CALL_START`/`ARGS`/`END`/`RESULT`, `STATE_SNAPSHOT`/`STATE_DELTA` (an RFC 6902 patch), `CUSTOM` and `RAW` (`schema.json:46`; `events/state.mdx:19-21`).
- A consumer "MUST NOT act on the arguments before `TOOL_CALL_END`" (`tool-calls.mdx:23-92`).

**Interrupts and generative UI.**
- Interrupts are normative in 1.0. A run finishes with `Interrupt` objects carrying a `responseSchema`, and the client resumes with `resume` (`basic/patterns/interrupt-resume.mdx:11-30`).
- Generative UI is still a draft (`drafts/generative-ui.mdx`).

**Worth having.**
- An `open` verb is a typed handoff to the client, which is the same idea as a frontend tool: the client executes it, the server only names it.
- Results are answers keyed by call id, and a decline is still an answer.

**Not needed.**
- In AG-UI the client defines tools for an agent. In flotilla the daemon and data files define verbs, and the client only implements a fixed set of `open` relations.
- The streaming event model and generative UI belong to a chat-driven agent loop. Verb results in flotilla arrive as row changes on watched tables.

## MCP

**Tools** (spec 2026-07-28, `server/tools.mdx`).
- A tool has `name`, `title`, `description`, `inputSchema` (a JSON Schema object, required), `outputSchema` and `annotations` (`:282-303`).
- Names SHOULD be 1-128 characters from `A-Za-z0-9_-.` and unique per server (`:309-330`).

**Annotations are hints.**
- They are `readOnlyHint` (default false), `destructiveHint` (default true), `idempotentHint` and `openWorldHint` (default true).
- Clients "MUST consider tool annotations to be untrusted unless they come from trusted servers" (`:305-306`; `schema.ts:1900-1954`).

**Listing and errors.**
- `tools/list` returns tools "currently available to the requesting client". It may vary by authorisation but not by connection, and SHOULD be deterministic for caching (`:62-69`; changelog §Minor 3).
- A tool failure is a result with `isError: true`, which the model sees. Protocol errors are for unknown tools or malformed input (`:738-785`).

**Elicitation.** Its `requestedSchema` is "limited to flat objects with primitive properties only" (`client/elicitation.mdx:119-164`).

**Bearing on flotilla.**
- A `command` verb projects directly onto a tool.
- MCP has no applicability per target. The agent supplies the subject key as input, and an inapplicable call comes back as `isError`.
- Pick-to-fill does not exist for an agent. A subject-kind parameter becomes a key string, ideally described with the table to look it up in.
- Elicitation's flat primitive schemas suggest keeping verb parameters flat and primitive as well.

## Brief: Plan 9 plumbing, macOS Services and App Intents, Redfish

**Plan 9 plumb(6).**
- A rule set is a set of patterns (`data matches`, `arg isfile`, `type is`) plus actions (`plumb to <port>`, `plumb start <cmd>`). The first rule set to match wins.
- The type is decided by matching the text, not declared.
- This is the closest analogue to `open`: a local handoff routed by a small rule file to whichever port is listening.

**macOS Services.**
- A service declares `NSSendTypes` and `NSReturnTypes`.
- A menu item is enabled when a responder answers `validRequestorForSendType:returnType:` for the current selection: local, typed, cheap applicability (Services Implementation Guide, "Validating Services Menu Items").

**App Intents.**
- `AppIntent` has `@Parameter` inputs.
- Parameters can be `AppEntity` types resolved through an `EntityQuery`. This is Apple's version of "parameter of subject kind, filled from a list".
- `requestConfirmation(...)` exists for confirming before performing.
- `parameterSummary` gives a one-line sentence form.
- `openAppWhenRun` was deprecated at 26.0 in favour of `supportedModes`, a split between run-in-background and hand-off-to-UI.

**Redfish.**
- Action parameters carry `@Redfish.AllowableValues` (`RedfishExtensions_v1.xml:36`), or an `ActionInfo` link per resource instance, because supported parameters "can differ among vendors and even among resource instances" (`ActionInfo.v1_5_0.json`).
- That is per-instance description again, and it costs a fetch.

---

## The operator's three questions

### 1. What a client needs from a verb description to render and invoke it with no per-verb code

| Need | Prior art | flotilla field |
|---|---|---|
| Stable name for binding and invocation | VisiData longname; MCP `name`; Siren `name` | `name` |
| Label and one-line help | Siren `title`; HAL-FORMS `title`/`prompt`; CLIM `:documentation`; VisiData helpstr | `label`, `help`; `description` for MCP |
| Suggested key | VisiData `addCommand` keystrokes, rebindable | `key` (a hint only) |
| Is it the default action | CLIM `:select`; Embark `embark-dwim` | `default: true`, at most one per kind |
| Applicability | CLIM tester/enabled; Services validation; Siren presence | `when`: conditions over row columns |
| Confirmation | Embark `embark--confirm`; App Intents `requestConfirmation` | `confirm` text with `{column}` substitution |
| Whether it changes state | MCP `readOnlyHint`/`destructiveHint` | `effect`: `read` / `write` / `destructive` |
| Parameters as typed widgets | Siren/HAL-FORMS `type`; JSON Schema | `params[].type`: string, text, integer, bool, enum |
| Enumerated options | HAL-FORMS `options.inline`/`link`; Redfish AllowableValues | `options` inline, or a `code` column's categories |
| Pick a row | CLIM input context; App Intents `AppEntity` | `type: {kind: <subject kind>}` with a `pick` view |
| Defaults from the row | CLIM argument `:default`; translator body returning args | `default: $row.<column>` |
| Prompting only for what's missing | CLIM partial commands; Embark injects only the first prompt | the client builds one form from the unbound params |
| Many rows | Embark `embark-act-all`/multitarget; VisiData `gd` | `picks: one \| each \| set` |
| Where the result goes | AG-UI tool message by call id; MCP `isError` | rows settle on watched tables; errors against the invocation id; `result.subject` is linkable |

### 2. How verbs reach a client over HTTP, and the same verbs as MCP tools

There are two hypermedia answers.

**Per resource** (Siren `actions`, HAL-FORMS `_templates`, Redfish `ActionInfo`):
- The server decides availability for each representation.
- This is right for a single document a person navigates.
- For a watched table of hundreds of rows it multiplies payload and server work by the row count and by every change. It also gives the client nothing to grey out.

**Per type** (CLIM command tables, Embark `embark-keymap-alist`, VisiData's per-sheet-type commands, MCP `tools/list`):
- The catalogue is fetched once and evaluated locally per row.
- MCP explicitly makes `tools/list` deterministic and cacheable.

flotilla should use the per-type form. Decision 5 of #4105 already says anything listed is a table, so the catalogue is a `verbs` table:
- keyed by subject kind and verb name;
- linked from each table description;
- served and watched like any other table.

Invocation goes through one generic route, for example `POST /verbs/{name}` with `{subjects: [...], params: {...}}`. No verb needs its own method or href.

The MCP server serves the `command` verbs as tools from the same rows:
- the name becomes the tool name;
- `help`/`description` become `description`;
- the subject key plus the params become a flat `inputSchema`;
- `effect` becomes the annotations.

`open` verbs are not offered to agents, since an agent has no screen to hand off to.

### 3. How a client learns which verbs apply to a row without a round trip per row

There are four observed designs.

1. **Server includes or omits per row** (Siren, HAL-FORMS, Redfish). There is no round trip only because the cost is moved into every row payload, and the client cannot grey out.
2. **Arbitrary local predicate code** (CLIM testers). There is no round trip, but it is code, it runs on every pointer move, and it was the main performance warning in Dynamic Windows' own documentation.
3. **The type alone** (Embark keymaps, VisiData sheet types). Cheap, but too coarse: abandon must not be offered on an `Abandoned` convoy.
4. **Typed declarative validation by the holder of the selection** (macOS Services: send types plus a local validation call).

flotilla can combine 3 and 4:
- the kind selects the verb set;
- a declarative condition over the row's columns, in the same grammar as `where`, selects within it;
- the client evaluates it locally and greys out a verb whose condition fails;
- the server re-checks at invocation, since a row can change between render and click.

Server-only facts the client cannot compute should be published as ordinary columns, for example `status.stalled`, which is already queryable on convoys (`crates/flotilla-resources/src/convoy.rs:36-50`). They should not be per-row verb lists.

---

## Today in flotilla (being replaced; described only)

**Per-table action specs in TUI code.**
- `ActionSpec<R>` is `{id, label, key, resolve: fn(&R) -> Option<TableIntent>}` (`crates/flotilla-tui/src/table_view.rs:352-357`).
- `TableSpec::project` runs every `resolve` for every row and keeps those that return `Some` as `AvailableAction`s on the `ProjectedRow` (`table_view.rs:368-395, 432-447`).
- Applicability is therefore Rust code per row, as with CLIM testers, and the result is per-row lists, as with Siren.
- Convoys have `open_pr`, `delete` and `open_in_pm` (`table_view.rs:1032-1036`). Vessels have `attach`, `force_complete` and `open_in_pm` (`:1084-1088`). Issues have `start` (`:1162-1163`).

**`TableIntent`** is a closed enum of eight intents (`table_view.rs:413-422`): `OpenInPm`, `AttachPane`, `DeleteConvoy`, `OpenChangeRequest`, `ForceCompleteWork`, and three `StartConvoy` forms.
- `execute_table_intent` turns each into either a local effect or a daemon `CommandAction` (`crates/flotilla-tui/src/app/key_handlers.rs:146-205`).
  - `OpenInPm` is a local focus report.
  - `AttachPane` becomes `AttachTransient`.
  - `DeleteConvoy` and the start intents push bespoke confirm modals (`ConvoyDeleteConfirmWidget`, `DispatchConfirmWidget`).
- The `open` versus `command` split exists, but only implicitly.

**Action menu.**
- `.` opens `TableActionMenuWidget` on the selected row's actions (`crates/flotilla-tui/src/binding_table.rs:150`; `crates/flotilla-tui/src/widgets/table.rs:385-386`).
- The menu lists `[key] label`, and runs an action on Enter, its key, or a click (`crates/flotilla-tui/src/widgets/table_action_menu.rs:16-33, 52-82, 84-93`).
- Its key mode is `ActionMenu` (`binding_table.rs:175-180`), rebindable through `action_menu` in config (`crates/flotilla-config/src/lib.rs:135`).
- Enter on a row drills if it can, else runs the row's only action if there is exactly one (`widgets/table.rs:46-53`). That is an accidental default action.
- Multi-pick exists only for issue starts (`widgets/project_page.rs:267-273`).

**Descriptors.**
- flotillad serves Frictionless-style table descriptions built from `TableSet`/`Table`/`ColumnDef` (`crates/flotilla-schema/src/descriptor.rs:41, 82, 185`; `docs/tables.md` §Description).
- Columns carry `type`, `categories` for `code` columns, `reference`/`foreignKeys`, and `properties` (`roles`, `title`, `sortable`, `whereOperators`).
- Filtering is `where=<field> <op> <literal>`, repeatable and all must hold, in the grammar of `crates/flotilla-address/src/condition.rs`. Its operators are `== != < <= > >=` (`condition.rs:19-46`), with no `or` and no `in`.
- Resource-kind columns advertise only `==`, and only on their declared queryable columns (`docs/tables.md` §Properties in use).
- Descriptors carry no actions today.

**Command applicability.** A separate exhaustive match in `crates/flotilla-commands/src/applicability.rs:22-25` decides which `CommandAction`s the TUI palette may dispatch. This is policy in code, by command rather than by row.

---

## Recommendation for flotilla's verb document

### Fields

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Stable dotted name, `<kind-singular>.<verb>`, for example `convoy.abandon`. It is used for key binding, invocation and the MCP tool name, so it uses only MCP-legal characters. |
| `subject` | yes | The subject kind, which is the table name, for example `convoys`. Dispatch is single, on this kind. |
| `label` | yes | Menu text. |
| `help` | yes | One line, for the status line, palette and pointer documentation. |
| `description` | no | Longer text for MCP `description`. Defaults to `help`. |
| `key` | no | A suggested key. Clients may rebind it (VisiData `bindkey`). |
| `default` | no | `true` on at most one verb per kind. It runs on activate (Enter or double-click). The server rejects a second default at load. |
| `kind` | yes | `command` or `open`. |
| `when` | no | A list of conditions, all of which must hold, in the `where` grammar over the subject table's columns. Omitted means always applicable. |
| `picks` | no | `one` (default), `each` or `set`. See "Arity" below. |
| `effect` | yes for `command` | `read`, `write` or `destructive`. A read-only view hides `write` and `destructive` verbs. Maps to MCP annotations. |
| `confirm` | no | Confirmation text with `{column}` substitution. Required when `effect: destructive`. For several picks the client prefixes the count, as in Embark's "Run %s on %d %ss?". |
| `params` | no | A list of typed parameters (below). |
| `command` | for `command` | `{action: <daemon command>, args: {<field>: <param or $row.column>}}`. |
| `open` | for `open` | `{relation: <registry name>, subject: {kind, key: [$row.columns…]}, href?: <template>}`. |
| `result` | no | `{subject: <kind>}` when the command names a new or other subject that the client may focus or drill into. |

`reads`, the list of columns a verb needs, is **derived** by the server from `when`, the `$row.` bindings and `confirm`, and published in the catalogue row. It is not authored. This gives binnacle what it asks for (a hidden column still feeds the verb) without a field that can drift.

### Parameter typing

The scalar types are `string`, `text` (multi-line), `integer`, `number`, `bool` and `enum`. Keep them flat and primitive, as MCP elicitation does, so that each parameter maps onto a single widget and a JSON Schema property.

Each parameter has:
- `name`, `label`, `type`, `required` and `default`;
- optional `options` for `enum` (inline values). A parameter whose values are a `code` column's categories names that column instead: `options: {column: convoys.status.phase}`.

`default` is a literal or `$row.<column>`. The client prompts only for parameters that are unbound and required or editable: CLIM's partial-command rule.

**Subject-kind parameters.** These are written `type: {kind: vessels}`, with an optional `pick: {view: <view name>, where: [...]}`.
- The client opens that view in pick mode, and the pick returns the chosen row's key.
- The `where` may reference `$row.`, for example only the vessels of this convoy.
- Over MCP the parameter becomes a string property described as "key of a row in `vessels`".

This is CLIM's input context without the type lattice, and App Intents' `AppEntity`/`EntityQuery`.

Leave out `regex`, min/max and step. They are cheap to add later if a real verb needs them. The server validates in any case.

### Applicability

- `when` uses the existing condition grammar, the one `where` parses (`crates/flotilla-address/src/condition.rs`); it is not a second grammar. The client evaluates it against the row's cells, and the server evaluates it again on invocation, answering `not_applicable` with the failing condition.
- The client **greys out** verbs whose `when` fails, with the failing condition as the tooltip, and hides only verbs of unknown `kind`/`relation` or verbs blocked by the view's read-only status.
- The grammar lacks `in` and `or`. For now, state sets are written as a conjunction of `!=`.
  - Add `in` to `condition.rs` when the first verb needs a positive set. It is a small, local addition.
  - Do not add arbitrary predicates.
- Client evaluation must not be limited by a column's `whereOperators`, which is about server-side filtering support. The client evaluator accepts every operator on every scalar column.

### The `command` and `open` kinds

**`command`** names an existing daemon command and binds its arguments from `$row.` columns and parameters. This follows Embark: actions are existing commands with the target injected.
- There is no new code per verb, and the CLI, TUI, Wheelhouse and MCP invoke the same command.
- The result settles through the watched tables. The client marks the row pending until an `upsert` or `delete` arrives, or the invocation fails.

**`open`** names a relation from a small registry and carries the subject identity (kind plus key columns), plus an optional `href` template as a fallback.
- The registry is a data file shipped with flotilla. It starts with `terminal` (attach), `focus` (focus a window or pane), `workspace` and `url`.
- A client implements the relations it can. It hides a verb whose relation it does not know, rather than failing.
- Nothing is sent to flotillad unless the client's implementation of the relation does so. For example, a TUI `terminal` relation may itself issue `AttachTransient`, as `AttachPane` does today.

### Arity: one row versus the picks

The two client lists disagree here:
- Wheelhouse wants one invocation over the picks, or a declared fan-out.
- binnacle wants separate one-row and picked-rows entries, as VisiData has `d` and `gd`.

The evidence:
- Embark writes each action once. `embark-act` and `embark-act-all` reuse the same keymap, and the batch asks for confirmation once (`embark.el:2483-2558`).
- VisiData writes the `g` variant by hand, per command and per sheet type.
- CLIM has no multi-pick at all.

**Recommendation:** one verb record with `picks`.
- `one`: the verb takes only the cursor row.
- `each`: the verb applies per row. The client sends **one** invocation with all picked subjects and gets per-subject outcomes back; the server fans out. This is Wheelhouse's "declared fan-out" as a single call.
- `set`: the command itself takes the list, as Embark's multitarget actions do, for example batch convoy start.

Clients that want VisiData-style separate entries derive them. A verb with `picks: each|set` appears twice in the menu, as "Abandon" (cursor row) and "Abandon 5 picked", under a key such as the verb's key plus a picks modifier. Each entry evaluates `when` on every picked row, and the picked entry is greyed if any pick fails.

This meets binnacle's need, two entries and two bindings, without doubling the data file. Binding names are `<name>` and `<name>` plus a picks marker, which is the client's convention.

### MCP projection

Only `command` verbs are projected.

| MCP field | Comes from |
|---|---|
| `name` | `name` |
| `title` | `label` |
| `description` | `description`, followed by the `when` conditions as text so the agent knows when the call will fail |
| `inputSchema` | An object with the subject key columns as required string properties, or `subjects: array` of key objects for `each`/`set`, plus one property per param. `enum` becomes `enum`. A subject-kind param becomes a string with its kind in the description. |
| `readOnlyHint` | `effect == read` |
| `destructiveHint` | `effect == destructive` |
| `idempotentHint` | Left at its default unless a verb sets it. |
| `openWorldHint` | `false` |

An inapplicable or failed call returns `isError: true` with the server's message.

`confirm` does not become elicitation. Whether an agent may run a destructive verb is a permission decision, and MCP says annotations are untrusted hints.

### HTTP delivery

- Verbs are rows of a `verbs` table: one row per verb, keyed by `subject` and `name`.
- Each table description links to it filtered by its kind, for example `/tables/verbs/rows?where=subject == convoys`. A client fetches and watches it once per kind it shows.
- Invocation is one route, `POST /verbs/{name}`, with `{subjects, params}`. It returns an invocation id and per-subject outcomes, or `202` and later outcomes.
- The open-relation registry is a second small table, `open_relations`.

### Client asks: Wheelhouse and binnacle

Wheelhouse's list is from the Actions and edits section of flotilla-org/wheelhouse#505. binnacle's list is from its viewer survey (`docs/research/11`, #289 section, branch `research/described-tables`); it was relayed, not read directly.

**Wheelhouse**

| # | Ask | Answer | Why |
|---|---|---|---|
| 1 | Open verbs carry the subject identity, not only a URL | **Adopted.** `open.subject` is kind plus key; `href` is an optional fallback. | Plan 9 plumbing and AG-UI frontend tools both hand the receiver a typed thing and let it decide locally. |
| 2 | Shared registry of open relations; unknown ones are hidden or disabled | **Adopted.** It is the `open_relations` table, and unknown relations are hidden. | It keeps `open` closed and data-only. AG-UI's client-defined tools are the alternative this avoids. |
| 3 | Applicability without a per-row round trip, evaluable from cells or published per row; grey out | **Adopted, minus "published per row".** Conditions over cells; server-only facts become columns. | Siren/HAL-FORMS per-row actions cost every row and cannot express "greyed". CLIM grey-out comes from enable notifications (spec 28.4). |
| 4 | Picks: one invocation, or a declared fan-out | **Adopted.** `picks: each` is one invocation with server fan-out; `set` passes the list to the command. | Embark `embark-act-all` and multitarget actions. |
| 5 | Confirm hint, optionally with a server-computed impact summary | **Partly.** `confirm` with substitution is adopted; an impact summary is deferred. | Embark and App Intents confirm with local text. No prior art computes an impact summary before asking. When needed, express it as a count over a related table (for example vessels under this convoy), not as a new verb endpoint. |
| 6 | Effects as row changes; errors against the invocation; a result naming a subject is linkable | **Adopted.** Rows settle through the watch, errors are keyed by invocation id, and `result.subject` is linkable. | AG-UI keys answers by call id; MCP keeps tool errors as results. |
| 7 | A pick parameter names the table or presentation to pick from | **Adopted.** `type: {kind}` plus `pick.view`. | CLIM input contexts; App Intents `EntityQuery`. |
| 8 | Edits are setter verbs on editable columns | **Adopted.** A column property `setter: <verb name>`. The setter takes `$row` keys and one param named `value`, typed by the column. | Decision 6 of #4105. It needs no new mechanism. |

Wheelhouse's grilling also proposed "a method plus URL template for `command`" for each verb. That is **rejected** in favour of one invoke route. Siren and HAL-FORMS need a method and href per action because they have no shared route. flotilla does, and per-verb URLs would duplicate the router.

**binnacle**

| # | Ask | Answer | Why |
|---|---|---|---|
| 1 | Stable name, label, one-line help, suggested key | **Adopted:** `name`, `label`, `help`, `key`. | VisiData `addCommand` and `bindkey`. |
| 2 | Arity as separate entries, not one verb with a mode | **Partly.** One record; clients derive two entries. | VisiData shows separate entries are useful in the UI. Embark shows one definition is enough. Doubling the data file has no benefit. |
| 3 | Condition in the `where` grammar; declare the columns read | **Adopted.** `reads` is derived by the server, not authored. | Derived data cannot drift from `when`. |
| 4 | Confirm text with row values; a flag for state change | **Adopted.** `confirm` with `{column}`; `effect` with three values. | MCP's read-only and destructive hints. |
| 5 | Typed params; a subject-kind param names a view to open in pick mode | **Adopted.** | Same as Wheelhouse 7. |
| 6 | Commands settle through the stream; opens go to the host; say whether a result drills | **Adopted.** `result.subject` covers the drill. | Same as Wheelhouse 6. |
| 7 | At most one default verb per kind, run on activate | **Adopted**, and enforced at load. | CLIM `:select`; `embark-dwim`. It replaces today's "the only action" accident (`widgets/table.rs:46-53`). |
| 8 | Verbs with or next to the description, or a verbs table, learned once per table | **Adopted:** a `verbs` table linked from the description. | MCP's `tools/list` is a cacheable per-type catalogue; decision 5 of #4105 says anything listed is a table. |

### Leave out

- **Per-row, server-computed action lists** (Siren, HAL-FORMS, Redfish `ActionInfo`). They cost payload and evaluation for every row on every change, and they lose "greyed". Server-only facts become columns instead.
- **A subject-kind inheritance lattice** (CLIM presentation types). Its parameter massaging, `or`/`and` breakage and inconsistent `typep` are the best-documented trouble spots in CLIM. Dispatch stays on the exact address kind. Shared verbs are written per kind, or generated by a loader if repetition becomes painful.
- **Gesture-keyed translator matrices and priorities.** Each kind needs one default and a menu. CLIM's ordering rules were "complicated" by Franz's own account.
- **Arbitrary predicate code for applicability** (CLIM testers). Dynamic Windows' own documentation warns about it. Conditions stay declarative, and the server re-checks.
- **Per-view or per-client rendering methods** (CLIM `accept`/`present` by view). Clients render from parameter types.
- **Multiple dispatch** (on the row kind and a parameter's kind together). No current verb needs it, and #4105 leaves it open.
- **Client-defined verbs** (AG-UI frontend tools). Clients implement `open` relations from the registry; they do not add verbs.
- **HTTP method and href per verb** (Siren, HAL-FORMS). One invoke route is enough.
- **Streaming generative UI, the AG-UI event model, and mid-command interrupts or elicitation.** Results arrive as table rows. A verb that needs more input should declare it as params.
- **Embark-style pre/post/around hooks.** Confirmation is a field, and anything else is the command's job on the server.
- **Validation attributes beyond type, required and options** (HAL-FORMS `regex`, `min`/`max`, `step`). The server validates. Add them only when a verb needs them.

### Examples

The column names are from `convoys` (`metadata.name`, `metadata.namespace`, `status.phase`, `status.stalled`; `crates/flotilla-resources/src/convoy.rs:28-50`). Command arguments are from `ConvoyAbandon` and `ConvoyResume` (`crates/flotilla-protocol/src/commands.rs:1062-1088`) and the CLI arguments (`crates/flotilla-commands/src/commands/convoy.rs:86-125`). Phases are from `ConvoyPhase` (`crates/flotilla-resources/src/convoy.rs:1589-1607`), where `Landed`, `Failed` and `Abandoned` are terminal.

```yaml
# verbs/convoys.yaml
- name: convoy.abandon
  subject: convoys
  label: Abandon convoy
  help: Archive work best-effort and tear the convoy down
  key: A
  kind: command
  picks: each                      # one invocation over the picks; server fans out
  effect: destructive
  when:                            # not terminal (ConvoyPhase::is_terminal)
    - status.phase != Landed
    - status.phase != Failed
    - status.phase != Abandoned
  confirm: "Abandon {metadata.name}? Uncommitted work may be lost."
  params:
    - name: reason
      label: Reason
      type: text
      required: true               # CLI: --reason, must be non-empty
  command:
    action: ConvoyAbandon
    args:
      namespace: $row.metadata.namespace
      name: $row.metadata.name
      reason: $param.reason

- name: convoy.resume
  subject: convoys
  label: Resume crew
  help: Send the crew a message; reopen to reactivate a settled completion
  key: r
  kind: command
  picks: one
  effect: write
  when:                            # GUESS: which phases accept a resume
    - status.phase != Pending
    - status.phase != Failed
    - status.phase != Abandoned
  params:
    - name: prompt
      label: Message
      type: text
      required: true
    - name: vessel
      label: Vessel
      type: {kind: vessels}
      required: false              # inferred server-side when exactly one matches
      pick:
        view: vessels.default      # GUESS: view name
        where:
          - spec.convoy_ref == $row.metadata.name   # GUESS: vessels lists spec.convoy_ref as filterable
    - name: reopen
      label: Reopen accepted completion
      type: bool
      default: false
  command:
    action: ConvoyResume
    args:
      namespace: $row.metadata.namespace
      name: $row.metadata.name
      prompt: $param.prompt
      vessel: $param.vessel
      reopen: $param.reopen
```

An `open` verb on terminal sessions (`terminalsessions`, `crates/flotilla-resources/src/terminal_session.rs:25-40`, phases at `:328-335`):

```yaml
- name: terminalsession.attach
  subject: terminalsessions
  label: Attach
  help: Attach to this session's terminal
  key: a
  default: true                    # Enter on a session row attaches
  kind: open
  when:
    - status.phase == Running
  open:
    relation: terminal
    subject: {kind: terminalsessions, key: [$row.metadata.namespace, $row.metadata.name]}
```

Guesses and gaps in these examples:
- **The `resume` applicability is a guess.** The CLI accepts resume in any phase and the server decides. Settle it from the crew executor before shipping.
- **`spec.convoy_ref` is not filterable today.** It is a real `VesselSpec` field, but vessels declare only `metadata.name` and `status.phase` as queryable (`crates/flotilla-resources/src/vessel.rs:21`). It would need adding.
- **`withdraw` is left out of `convoy.resume`.** The CLI's `--withdraw` maps to a different command (`ConvoyWithdrawPendingBrief`), so it should be its own verb, `convoy.withdraw_brief`, not a mode.
- **The `terminal` relation's key is a guess.** Today's TUI attach uses a vessel's `materialize_ref` and host (`table_view.rs:1508-1510`). Whether the relation needs the host as part of the key, or resolves it from the session row, is open.
