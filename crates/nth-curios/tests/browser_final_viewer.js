// Run each stage on a fresh browser_serve host, at desktop and 390px widths:
// agent-browser eval 'window.CURIOS_TEST_STAGE="save"'
// agent-browser eval "$(<crates/nth-curios/tests/browser_final_viewer.js)"
// agent-browser reload; repeat with stage "send". Touch the fixture's revise
// file, wait for r2, then repeat with stage "revision". Each eval must pass.
(async () => {
  const $ = (id) => document.getElementById(id);
  const results = [];
  const assert = (name, ok) => {
    if (!ok) throw new Error(name);
    results.push(name);
  };
  const waitFor = async (name, predicate) => {
    for (let i = 0; i < 100; i++) {
      if (await predicate()) return;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error(`Timed out: ${name}`);
  };
  const state = async (revision) => {
    const response = await fetch(
      `/api/state${revision ? `?revision=${revision}` : ""}`,
      {
        headers: { "X-Curios-Request": "viewer" },
      },
    );
    if (!response.ok)
      throw new Error(`State request failed: ${response.status}`);
    return response.json();
  };
  const thread = (snapshot, id) =>
    snapshot.threads.find((t) => t.target.element_id === id);
  const messages = (snapshot) => snapshot.threads.flatMap((t) => t.messages);
  const sendButton = () =>
    $("send-all") ||
    [...$("dock").querySelectorAll("button")].find((button) =>
      /send all/i.test(
        `${button.textContent} ${button.getAttribute("aria-label")}`,
      ),
    );
  const input = (text) => {
    $("comment-body").value = text;
    $("comment-body").dispatchEvent(new Event("input", { bubbles: true }));
  };
  const enter = (shiftKey = false) =>
    $("comment-body").dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Enter",
        shiftKey,
        bubbles: true,
        cancelable: true,
      }),
    );
  const escape = () =>
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
    );
  const doc = () => $("document").contentDocument;
  const open = (id) => doc().getElementById(id).click();
  const pin = (t) => document.querySelector(`.pin[data-id="${t.id}"]`);
  const savedCount = (s) =>
    messages(s).filter((m) => m.delivery === "saved").length;
  await waitFor(
    "viewer ready",
    () => doc()?.getElementById("heading") && sendButton(),
  );
  const button = sendButton();

  if (window.CURIOS_TEST_STAGE === "save") {
    assert("Send all starts disabled", button.disabled);
    doc().body.click();
    assert("Unanchored click opens nothing", $("popover").hidden);
    open("heading");
    input("Retained anchor feedback");
    escape();
    open("heading");
    assert(
      "Escape preserves the draft",
      $("comment-body").value === "Retained anchor feedback",
    );
    enter();
    await waitFor(
      "first saved comment",
      async () => savedCount(await state()) === 1 && !button.disabled,
    );
    let snapshot = await state();
    const retained = thread(snapshot, "heading");
    await waitFor("pending pin", () => pin(retained));
    assert(
      "Enter saves without notifying nth",
      retained.messages[0].delivery === "saved",
    );
    assert(
      "Pending thread is not working",
      !pin(retained).classList.contains("working") && $("working").hidden,
    );
    assert(
      "Pending message is labeled",
      /pending/i.test($("messages").innerText),
    );
    escape();
    open("summary");
    input("Removed anchor feedback");
    enter();
    await waitFor(
      "second saved comment",
      async () => savedCount(await state()) === 2,
    );
    await waitFor("pending count", () =>
      /2/.test(`${button.textContent} ${button.getAttribute("aria-label")}`),
    );
    assert("Dock shows two pending comments", !button.disabled);
    assert(
      "Dock fits the viewport",
      document.documentElement.scrollWidth <= innerWidth,
    );
    return results;
  }

  if (window.CURIOS_TEST_STAGE === "send") {
    let snapshot = await state();
    assert("Reload retains both saved comments", savedCount(snapshot) === 2);
    await waitFor("count after reload", () =>
      /2/.test(`${button.textContent} ${button.getAttribute("aria-label")}`),
    );
    const retained = thread(snapshot, "heading");
    await waitFor("retained pin", () => pin(retained));
    pin(retained).click();
    input("Composer included by dock");
    // Fail one send before it reaches the host, then retry the saved batch.
    const originalFetch = window.fetch;
    let failed = false;
    const sends = [];
    window.fetch = function (path, options) {
      if (String(path).endsWith("/api/send"))
        sends.push(JSON.parse(options.body));
      if (!failed && String(path).endsWith("/api/send")) {
        failed = true;
        return Promise.resolve(
          new Response(JSON.stringify({ message: "Injected send failure" }), {
            status: 500,
            headers: { "Content-Type": "application/json" },
          }),
        );
      }
      return originalFetch.call(this, path, options);
    };
    try {
      button.click();
      await waitFor("failed send", () => failed && !button.disabled);
      snapshot = await state();
      assert(
        "Failed send retains all three comments",
        savedCount(snapshot) === 3,
      );
      assert(
        "Composer saved once before failure",
        messages(snapshot).filter((m) => m.body === "Composer included by dock")
          .length === 1,
      );
      button.click();
      await waitFor(
        "retry sends batch",
        async () => savedCount(await state()) === 0,
      );
      assert(
        "Retry submits one batch with the original request ID and three messages",
        sends.length === 2 &&
          sends[0].request_id === sends[1].request_id &&
          sends[1].message_ids.length === 3,
      );
    } finally {
      window.fetch = originalFetch;
    }
    snapshot = await state();
    assert(
      "Retry does not duplicate the composer",
      messages(snapshot).filter((m) => m.body === "Composer included by dock")
        .length === 1,
    );
    assert(
      "All three comments are queued",
      messages(snapshot).filter((m) => m.delivery === "queued").length === 3,
    );
    await waitFor(
      "working indicator",
      () =>
        pin(retained)?.classList.contains("working") && !$("working").hidden,
    );
    input("Unsent reply while waiting");
    enter();
    await waitFor(
      "new saved reply",
      async () => savedCount(await state()) === 1,
    );
    assert(
      "An unsent reply does not hide outstanding work",
      pin(retained).classList.contains("working") && !$("working").hidden,
    );
    escape();
    open("summary");
    input("Shift sends the whole batch");
    enter(true);
    await waitFor("Shift+Enter sends both pending replies", async () => {
      const s = await state();
      return (
        savedCount(s) === 0 &&
        messages(s).filter((m) => m.delivery === "queued").length === 5
      );
    });
    assert("Shift+Enter includes the earlier pending reply", true);
    return results;
  }

  if (window.CURIOS_TEST_STAGE === "revision") {
    await waitFor(
      "revision two published",
      async () => (await state()).curio.current_revision === 2,
    );
    await waitFor(
      "latest revision control",
      () => !$("update").hidden && !$("update").disabled,
    );
    $("update").click();
    await waitFor(
      "revision two document",
      () =>
        doc()?.getElementById("heading")?.textContent.includes("Revised") &&
        $("current-revision").textContent === "r2",
    );
    const latest = await state(2);
    const retained = thread(latest, "heading"),
      removed = thread(latest, "summary");
    assert(
      "Changed label retains attachment",
      retained?.attachment !== "detached",
    );
    assert("Removed ID is detached", removed?.attachment === "detached");
    await waitFor("carried pin", () => pin(retained));
    assert("Removed anchor has no latest pin", !pin(removed));
    pin(retained).click();
    input("Reply on revision two");
    enter();
    await waitFor("reply on carried thread", async () =>
      thread(await state(2), "heading").messages.some(
        (m) => m.body === "Reply on revision two",
      ),
    );
    await waitFor(
      "reply reflected in viewer",
      () => /1/.test(button.textContent) && !button.disabled,
    );
    escape();
    const previous = [...$("revisions").querySelectorAll("button")].find(
      (b) => b.textContent.trim() === "r1",
    );
    await waitFor("revision selector ready", () => !previous.disabled);
    previous.click();
    await waitFor(
      "revision one document",
      () =>
        doc()?.getElementById("summary") &&
        $("current-revision").textContent === "r1",
    );
    const history = await state(1);
    await waitFor("historical pin", () => pin(thread(history, "summary")));
    assert(
      "Removed thread remains available on r1",
      Boolean(pin(thread(history, "summary"))),
    );
    return results;
  }
  throw new Error(
    'Set window.CURIOS_TEST_STAGE to "save", "send", or "revision"',
  );
})();
