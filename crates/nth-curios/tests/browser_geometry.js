// Run on browser_serve after saving a #nested pin and opening a default
// #nested region composer. This changes only the document fixture geometry.
// agent-browser eval "$(<crates/nth-curios/tests/browser_geometry.js)"
(async () => {
  const doc = document.querySelector("iframe").contentDocument;
  const scroller = doc.querySelector("#scroller");
  const nested = doc.querySelector("#nested");
  const results = [];
  const settle = () =>
    new Promise((resolve) =>
      requestAnimationFrame(() => requestAnimationFrame(resolve)),
    );
  function capture(name) {
    const bounds = scroller.getBoundingClientRect();
    const pin = document.querySelector(".pin");
    const outline = document.querySelector("#selection-box");
    results.push({
      name,
      target: nested.getBoundingClientRect().toJSON(),
      container: bounds.toJSON(),
      pin: pin
        ? {
            x: Number.parseFloat(pin.style.left),
            y: Number.parseFloat(pin.style.top),
            clip: pin.style.clipPath,
          }
        : null,
      outline: {
        hidden: outline.hidden,
        x: Number.parseFloat(outline.style.left),
        y: Number.parseFloat(outline.style.top),
        width: Number.parseFloat(outline.style.width),
        height: Number.parseFloat(outline.style.height),
      },
    });
  }
  for (const [name, y] of [
    ["hidden", 0],
    ["restored", 350],
    ["partial-bottom", 190],
    ["partial-top", 470],
  ]) {
    scroller.scrollTop = y;
    await settle();
    capture(name);
  }
  nested.style.width = "800px";
  nested.style.boxSizing = "border-box";
  nested.style.marginLeft = "500px";
  scroller.style.width = "420px";
  scroller.scrollTop = 350;
  for (const [name, x] of [
    ["horizontal-hidden", 0],
    ["horizontal-right", 150],
    ["horizontal-left", 700],
  ]) {
    scroller.scrollLeft = x;
    await settle();
    capture(name);
  }
  return results;
})();
