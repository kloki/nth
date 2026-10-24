// With the retained curios_e2e fixture open in agent-browser:
// agent-browser eval "$(<crates/nth-curios/tests/browser_roundtrip.js)"
// Read-only diagnostics plus deliberately rejected malformed requests.
// Never returns cookies, private handoff fragments or authorization headers.
(async () => {
  const header = { "X-Curios-Request": "viewer" };
  const response = await fetch("/api/state", { headers: header });
  if (!response.ok)
    throw new Error(`Authenticated snapshot: ${response.status}`);
  const state = await response.json();
  const statuses = {};
  for (const path of [
    "/api/state?revision=0",
    "/api/state?revision=abc",
    "/content/0",
    "/content/../state.json",
  ]) {
    statuses[path] = (await fetch(path, { headers: header })).status;
  }
  statuses.noViewerHeader = (await fetch("/api/state")).status;
  statuses.malformedComment = (
    await fetch("/api/comments", {
      method: "POST",
      headers: { ...header, "Content-Type": "application/json" },
      body: "{",
    })
  ).status;
  const resources = performance.getEntriesByType("resource");
  return {
    revision: document.getElementById("revision").value,
    latest: state.curio.current_revision,
    threads: state.threads.length,
    pending: state.threads
      .flatMap((t) => t.messages)
      .filter((m) => m.delivery === "saved").length,
    statuses,
    fragmentRemoved: location.hash === "",
    credentialNotScriptReadable: document.cookie === "",
    horizontalOverflow: document.documentElement.scrollWidth > innerWidth,
    externalResources: resources.filter(
      (r) => new URL(r.name).origin !== location.origin,
    ).length,
    resourceStatuses: [...new Set(resources.map((r) => r.responseStatus))],
  };
})();
