import assert from "node:assert/strict";

import { mergeLayers, noteIsStale } from "../dag-board.mjs";

const generated = {
  tickets: [
    { ref: "flotilla-org/flotilla#10", status: "landed", title: "Done" },
    { ref: "flotilla-org/flotilla#11", status: "ready", title: "Next" },
  ],
  dependencyEdges: [{ from: "flotilla-org/flotilla#10", to: "flotilla-org/flotilla#11" }],
  groups: { infrastructure: ["flotilla-org/flotilla#10"] },
};
const authored = {
  pulse: "The authored narrative stays authored.",
  tickets: {
    "flotilla-org/flotilla#10": { detail: "Expected to be underway.", statusAtAuthoring: "at-sea", shape: "diamond" },
  },
  groups: { infrastructure: { label: "Infrastructure", color: "#123456" } },
  edges: [{ from: "flotilla-org/flotilla#11", to: "flotilla-org/flotilla#10", label: "authored" }],
};

const board = mergeLayers(generated, authored);
const annotated = board.tickets.find((ticket) => ticket.ref === "flotilla-org/flotilla#10");
assert.equal(board.pulse, authored.pulse);
assert.equal(annotated.status, "landed", "generated status wins");
assert.equal(annotated.detail, authored.tickets["flotilla-org/flotilla#10"].detail);
assert.equal(annotated.noteStale, true);
assert.equal(board.groups.infrastructure.label, "Infrastructure");
assert.equal(board.edges.length, 2);
assert.equal(noteIsStale({ status: "ready" }, { detail: "new", statusAtAuthoring: "ready" }), false);
assert.equal(noteIsStale({ status: "ready" }, { detail: "old" }), false, "legacy notes without a status are not guessed stale");
