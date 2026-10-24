// Run on a fresh browser_serve host, then repeat at a 390px viewport:
// agent-browser eval "$(<crates/nth-curios/tests/browser_final_viewer.js)"
(async () => {
  const $ = (id) => document.getElementById(id);
  const doc = $("document").contentDocument;
  const results = [];
  const assert = (name, ok) => {
    if (!ok) throw new Error(name);
    results.push(name);
  };
  const settle = () =>
    new Promise((resolve) =>
      requestAnimationFrame(() => requestAnimationFrame(resolve)),
    );
  const add = (id) => {
    $("anchor").value = id;
    $("add-comment").click();
  };
  const type = (body) => {
    $("comment-body").value = body;
    $("comment-body").dispatchEvent(new Event("input"));
  };
  const escape = () =>
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  add("summary");
  type("Summary atomic draft");
  escape();
  assert(
    "Escape clears active textarea",
    $("comment-body").value === "" && $("composer").hidden,
  );
  add("footer");
  assert(
    "Footer starts empty",
    $("comment-body").value === "" &&
      $("target-label").textContent.includes("Bottom target"),
  );
  type("Footer independent draft");
  escape();
  const recover = [...$("draft-recovery").children].find((button) =>
    button.textContent.includes("Summary target"),
  );
  recover.click();
  assert(
    "Explicit recovery restores summary body and target",
    $("comment-body").value === "Summary atomic draft" &&
      $("target-label").textContent.includes("Summary target"),
  );
  assert(
    "Recovery preserves other draft",
    [...$("draft-recovery").children].some((button) =>
      button.textContent.includes("Bottom target"),
    ),
  );
  $("cancel-comment").click();

  // Use the real API and real rendering, including a deliberately long label.
  const response = await fetch("/api/comments", {
    method: "POST",
    headers: {
      "X-Curios-Request": "viewer",
      "Content-Type": "application/json",
    },
    body: JSON.stringify({
      request_id: crypto.randomUUID(),
      revision: 1,
      target: {
        kind: "element",
        element_id: "summary",
        label: "X".repeat(120),
      },
      body: "Geometry and wrapping probe",
    }),
  });
  assert("Real comment saved", response.ok);
  const deadline = Date.now() + 8000;
  while (
    !document
      .querySelector(".thread-card strong")
      ?.textContent.includes("X".repeat(120))
  ) {
    if (Date.now() > deadline) throw new Error("Thread rendering timeout");
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  window.scrollTo(0, 0);
  const summary = doc.getElementById("summary");
  const scroller = doc.getElementById("scroller");
  scroller.prepend(summary);
  summary.style.cssText =
    "position:fixed;top:10px;left:60px;width:260px;height:50px;padding:0;box-sizing:border-box";
  const check = async (name, visible) => {
    doc.dispatchEvent(new Event("scroll"));
    await settle();
    const box = summary.getBoundingClientRect();
    const hit = doc.elementFromPoint(box.left + 10, box.top + 10);
    assert(
      `${name}: browser hit test`,
      (hit === summary || summary.contains(hit)) === visible,
    );
    assert(`${name}: pin`, Boolean(document.querySelector(".pin")) === visible);
  };
  await check("Fixed escapes untransformed scroller", true);
  scroller.style.transform = "translateX(0)";
  await check("Fixed uses transformed containing block", true);
  summary.style.top = "-100px";
  await check("Transformed fixed clipped outside scroller", false);
  scroller.style.transform = "none";
  summary.style.position = "absolute";
  summary.style.top = "10px";
  await check("Absolute escapes static scroller", true);
  scroller.style.position = "relative";
  await check("Absolute uses positioned containing block", true);
  summary.style.top = "-100px";
  await check("Absolute clipped at positioned scroller", false);
  summary.style.position = "static";
  scroller.scrollTop = 0;
  await check("Ordinary nested visible", true);
  scroller.scrollTop = 350;
  await check("Ordinary nested clipped", false);
  assert(
    "No horizontal overflow",
    document.documentElement.scrollWidth <= innerWidth,
  );
  return {
    viewport: innerWidth,
    scrollWidth: document.documentElement.scrollWidth,
    results,
  };
})();
