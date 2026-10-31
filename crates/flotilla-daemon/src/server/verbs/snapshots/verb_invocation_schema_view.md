# VerbInvocation store schema view

Kind tables only; the shared tables are in `verb_invocation.sql`.

| Table | Role | Columns | From enum expansion | Child tables | Indexes |
|---|---|--:|---|---|---|
| `verb_invocation` | root | 7 | – | `verb_invocation_subjects` (child), `verb_invocation_outcomes` (child) | – |
| `verb_invocation_subjects` | child | 3 | – | – | – |
| `verb_invocation_outcomes` | child | 5 | – | – | – |

Tables wider than 30 columns:
- none
