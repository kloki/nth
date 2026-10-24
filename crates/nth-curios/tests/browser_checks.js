// Run after opening the browser_serve URL:
// agent-browser eval "$(<crates/nth-curios/tests/browser_checks.js)"
// These probes intentionally bypass publication validation to test browser policy itself.
(async () => {
  const frame = document.querySelector("#document");
  const doc = frame.contentDocument;
  const violations = [];
  doc.addEventListener("securitypolicyviolation", (event) =>
    violations.push([event.violatedDirective, event.blockedURI]),
  );
  const script = doc.createElement("script");
  script.textContent = "parent.document.body.dataset.browserScript='executed'";
  doc.body.append(script);
  const image = doc.createElement("img");
  image.src = "https://example.com/browser-probe.png";
  image.setAttribute(
    "onerror",
    "parent.document.body.dataset.browserHandler='executed'",
  );
  doc.body.append(image);
  const style = doc.createElement("style");
  style.textContent =
    "@import url(https://example.com/browser-probe.css);body{background-image:url(https://example.com/browser-background.png)}";
  doc.head.append(style);
  const svg = doc.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.innerHTML = '<image href="https://example.com/browser-svg.png"/>';
  doc.body.append(svg);
  const link = doc.createElement("a");
  link.href = "/api/state";
  link.textContent = "API navigation probe";
  doc.body.append(link);
  link.click();
  const unauthorized = await fetch("/api/state");
  const authorized = await fetch("/api/state", {
    headers: { "X-Curios-Request": "viewer" },
  });
  await new Promise((resolve) => setTimeout(resolve, 100));
  return {
    sandbox: frame.sandbox.value,
    parentCanTarget: Boolean(doc.querySelector("#summary, #replacement")),
    scriptExecuted: Boolean(document.body.dataset.browserScript),
    handlerExecuted: Boolean(document.body.dataset.browserHandler),
    violations,
    unauthorizedStatus: unauthorized.status,
    viewerCookieStatus: authorized.status,
    shellVisibleCookie: document.cookie,
    frameVisibleCookie: doc.cookie,
    parentHash: location.hash,
    frameHash: doc.defaultView.location.hash,
    tokenInStorage: [
      ...Object.keys(localStorage),
      ...Object.keys(sessionStorage),
    ].some((key) => /token|capability/i.test(key)),
    navigation: doc.defaultView.location.pathname,
    navigationNotice: document.querySelector("#notice").textContent,
    shellButtonBackground: getComputedStyle(document.querySelector("button"))
      .backgroundColor,
    invalidRegionInputs: [...document.querySelectorAll("#region-fields input")]
      .filter((input) => !input.checkValidity())
      .map((input) => ({
        id: input.id,
        value: input.value,
        message: input.validationMessage,
      })),
  };
})();
