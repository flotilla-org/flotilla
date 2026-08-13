const STATUS_ORDER = { "at-sea": 0, blocked: 1, ready: 2, landed: 3 };

export function noteIsStale(generatedTicket, authoredTicket = {}) {
  return Boolean(
    authoredTicket.detail &&
      authoredTicket.statusAtAuthoring &&
      authoredTicket.statusAtAuthoring !== generatedTicket.status,
  );
}

export function mergeLayers(generated, authored) {
  const authoredTickets = authored.tickets || {};
  const tickets = (generated.tickets || []).map((ticket) => {
    const annotation = authoredTickets[ticket.ref] || {};
    return {
      ...annotation,
      ...ticket,
      detail: annotation.detail || "",
      noteStale: noteIsStale(ticket, annotation),
    };
  });

  // Label names are external keys; a null prototype keeps names such as
  // `__proto__` as ordinary own properties.
  const groups = Object.create(null);
  for (const [name, refs] of Object.entries(generated.groups || {})) {
    groups[name] = { name, label: name, ticketRefs: [...refs], ...(authored.groups?.[name] || {}) };
  }
  for (const [name, group] of Object.entries(authored.groups || {})) {
    groups[name] ||= { name, label: name, ticketRefs: [], ...group };
  }

  const edges = [
    ...(generated.dependencyEdges || []).map((edge) => ({ ...edge, source: "generated" })),
    ...(authored.edges || []).map((edge) => ({ ...edge, source: "authored" })),
  ];
  const uniqueEdges = [...new Map(edges.map((edge) => [`${edge.from}\0${edge.to}\0${edge.label || ""}`, edge])).values()];

  return {
    ...generated,
    pulse: authored.pulse || "",
    tickets: tickets.sort(
      (left, right) =>
        (STATUS_ORDER[left.status] ?? 99) - (STATUS_ORDER[right.status] ?? 99) || left.ref.localeCompare(right.ref),
    ),
    groups,
    edges: uniqueEdges,
  };
}

function element(tag, attributes = {}, children = []) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (name === "className") node.className = value;
    else if (name === "text") node.textContent = value;
    else if (name === "style") Object.assign(node.style, value);
    else node.setAttribute(name, value);
  }
  for (const child of children) node.append(child);
  return node;
}

function ticketCard(ticket, edges) {
  const title = element("a", { href: ticket.url, text: ticket.title, target: "_blank", rel: "noreferrer" });
  const header = element("header", {}, [
    element("span", { className: `shape shape-${ticket.shape || "card"}`, "aria-hidden": "true" }),
    element("div", {}, [element("small", { text: ticket.ref }), title]),
    element("span", { className: `status status-${ticket.status}`, text: ticket.status }),
  ]);
  const children = [header];
  if (ticket.detail) {
    const noteClass = ticket.noteStale ? "detail stale" : "detail";
    const note = element("div", { className: noteClass });
    if (ticket.noteStale) {
      note.append(
        element("strong", {
          className: "stale-label",
          text: `STALE NOTE · was ${ticket.statusAtAuthoring}, now ${ticket.status}`,
        }),
      );
    }
    note.append(element("p", { text: ticket.detail }));
    children.push(note);
  }
  if (ticket.convoys?.length) {
    const convoyText = ticket.convoys
      .map((convoy) => `${convoy.name} on ${convoy.host || "unknown host"} · ${convoy.phase}`)
      .join("; ");
    children.push(element("p", { className: "meta", text: `⚓ ${convoyText}` }));
  }
  if (ticket.pullRequests?.length) {
    children.push(
      element("p", {
        className: "meta",
        text: ticket.pullRequests.map((pr) => `PR ${pr.ref?.split("#").at(-1) || "?"}: ${pr.state}, CI ${pr.ci}`).join("; "),
      }),
    );
  }
  const incoming = edges.filter((edge) => edge.to === ticket.ref);
  if (incoming.length) {
    children.push(
      element("p", {
        className: "dependencies",
        text: `Depends on ${incoming.map((edge) => edge.from).join(", ")}`,
      }),
    );
  }
  return element("article", { className: `ticket ticket-${ticket.status}` }, children);
}

export function renderBoard(root, board) {
  root.replaceChildren();
  if (board.pulse) {
    root.append(
      element("section", { className: "pulse" }, [element("span", { text: "PULSE" }), element("p", { text: board.pulse })]),
    );
  }
  const summary = element("section", { className: "summary", "aria-label": "Status summary" });
  for (const status of ["at-sea", "blocked", "ready", "landed"]) {
    summary.append(
      element("div", {}, [
        element("strong", { text: board.tickets.filter((ticket) => ticket.status === status).length }),
        element("span", { text: status }),
      ]),
    );
  }
  root.append(summary);

  const columns = element("section", { className: "columns" });
  const groupedRefs = new Set(Object.values(board.groups).flatMap((group) => group.ticketRefs));
  const groupEntries = Object.entries(board.groups);
  const uncategorized = board.tickets.filter((ticket) => !groupedRefs.has(ticket.ref)).map((ticket) => ticket.ref);
  if (uncategorized.length) groupEntries.push(["uncategorized", { label: "Uncategorized", ticketRefs: uncategorized }]);

  for (const [name, group] of groupEntries.sort((left, right) => left[1].label.localeCompare(right[1].label))) {
    const refs = new Set(group.ticketRefs);
    const tickets = board.tickets.filter((ticket) => refs.has(ticket.ref));
    if (!tickets.length) continue;
    const column = element("section", { className: "group", "data-group": name });
    column.style.setProperty("--group-color", group.color || "#8b9bb4");
    column.append(element("h2", { text: group.label }), ...tickets.map((ticket) => ticketCard(ticket, board.edges)));
    columns.append(column);
  }
  root.append(columns);
}

export async function loadBoard({ authoredUrl, generatedUrl }) {
  const load = async (url) => {
    const response = await fetch(url, { cache: "no-store" });
    if (!response.ok) throw new Error(`${url}: ${response.status} ${response.statusText}`);
    return response.json();
  };
  const [generated, authored] = await Promise.all([load(generatedUrl), load(authoredUrl)]);
  return mergeLayers(generated, authored);
}
