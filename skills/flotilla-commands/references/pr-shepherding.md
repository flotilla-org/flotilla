# PR shepherding

Deliver to the repository and target ref in the brief. Open a ready PR with the
closing-issue line, then use the forge's authenticated checks and review surface
(or its available shepherding skill) to inspect the current head. Handle every
finding, including nits and trailing sections, with a fix, concrete reasoning or
follow-up issue; reply to each finding you answer.

After each push, inspect checks and new reviews. Fix failures caused by the PR.
While checks are pending, report the PR URL and current state and yield at the
turn boundary. On the next delivered turn, inspect again. Rebase when the forge
reports a conflict or a required check fails because of a target-branch change.
Take a final snapshot with green settled checks, all review items answered and
no conflict. Read [settlement order](#settlement-order) for the rationale.

Inspect the convoy's work and claim evidence through the resource surface:

```sh
flotilla convoy CONVOY explain
flotilla resource get Convoy CONVOY
flotilla artifact list --convoy CONVOY --kind review-bundle
flotilla artifact get ARTIFACT_REFERENCE --output /tmp/review-bundle.md
```

Use the `crew-review` skill when assembling claim review evidence. Review the
claim's complete base-to-head pair and bind evidence to its current head digest.
Submit settlement evidence using [crew lifecycle](crew-lifecycle.md). Follow the
brief's merge authority; a delivery-only crew leaves merging to its supervisor.

## Settlement order

The final snapshot establishes readiness for the current head. Submit the ledger
only after that snapshot is clean, then complete. If new feedback or failed
checks arrive before completion, handle them and take a new clean snapshot before
resubmitting the ledger. Completion stores the crew's settlement claim; the
convoy's merge and landing can occur afterward.
