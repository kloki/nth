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
  const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  doc.body.click();
  assert("A click on no anchored element opens nothing", $("popover").hidden);
  doc.getElementById("summary").click();
  assert(
    "A click on an anchored element opens the popover",
    !$("popover").hidden,
  );
  assert("A new comment shows no messages", !$("messages").children.length);
  $("comment-body").value = "Tighten the summary";
  $("comment-body").dispatchEvent(new Event("input"));
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  assert("Escape closes the popover", $("popover").hidden);
  doc.getElementById("summary").click();
  assert(
    "The draft comes back",
    $("comment-body").value === "Tighten the summary",
  );
  $("comment-body").dispatchEvent(
    new KeyboardEvent("keydown", {
      key: "Enter",
      shiftKey: true,
      bubbles: true,
    }),
  );
  assert("Shift+Enter does not send", !document.querySelector(".pin"));
  $("comment-body").dispatchEvent(
    new KeyboardEvent("keydown", { key: "Enter", bubbles: true }),
  );
  await wait(800);
  const pin = document.querySelector(".pin");
  assert("Commenting adds a pin", Boolean(pin));
  assert("The pin is small", pin.offsetWidth <= 24);
  assert("A sent comment shows nth working", pin.classList.contains("working"));
  assert("The thread stays open", !$("popover").hidden && !$("working").hidden);
  assert(
    "The thread shows only who said what",
    $("messages").innerText.trim() === "You\nTighten the summary",
  );
  const state = await (
    await fetch("/api/state", { headers: { "X-Curios-Request": "viewer" } })
  ).json();
  assert(
    "The comment went to nth without a Send step",
    state.threads[0].messages[0].delivery === "queued",
  );
  pin.click();
  assert("A click on the open pin closes it", $("popover").hidden);
  return results;
})();
