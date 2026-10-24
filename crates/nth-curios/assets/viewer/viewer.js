(() => {
  "use strict";
  const $ = (id) => document.getElementById(id);
  const fragment = location.hash.slice(1);
  let token = fragment.startsWith("token=")
    ? new URLSearchParams(fragment).get("token")
    : fragment;
  history.replaceState(null, "", location.pathname + location.search);
  try {
    token = decodeURIComponent(token || "");
  } catch {
    token = "";
  }
  const requestedRevision = new URLSearchParams(location.search).get(
    "revision",
  );
  let revision =
    /^[1-9]\d*$/.test(requestedRevision || "") &&
    Number.isSafeInteger(Number(requestedRevision))
      ? Number(requestedRevision)
      : undefined;
  // `target` is the element a new comment is on; `selected` the thread
  // the popover shows. At most one of them is set.
  let state, loadedRevision, selected, target;
  let busy = false,
    authenticated = false,
    refreshing = false,
    refreshAgain = false;
  let frameCleanup = () => {},
    pinFrame = 0;
  const frame = $("document");
  const drafts = new Map();
  const requests = new Map();
  const HINT = "Click an element to comment.";
  const say = (text, error = false) => {
    $("notice").textContent = text;
    $("notice").classList.toggle("error", error);
  };
  function draftKey(key) {
    return `curios:${state.curio.session_id}:${state.curio.id}:${key}`;
  }
  function readDraft(key) {
    if (drafts.has(key)) return drafts.get(key);
    try {
      return sessionStorage.getItem(draftKey(key));
    } catch {
      return null;
    }
  }
  function writeDraft(key, value) {
    drafts.set(key, value);
    try {
      if (value) sessionStorage.setItem(draftKey(key), value);
      else sessionStorage.removeItem(draftKey(key));
    } catch {
      /* Recovery storage is optional. */
    }
  }
  function requestId(key, payload) {
    const signature = JSON.stringify(payload);
    if (requests.get(key)?.signature !== signature)
      requests.set(key, { signature, id: crypto.randomUUID() });
    return requests.get(key).id;
  }
  async function api(path, body) {
    const response = await fetch(path, {
      method: body ? "POST" : "GET",
      credentials: "same-origin",
      cache: "no-store",
      headers: {
        "X-Curios-Request": "viewer",
        ...(token ? { "X-Curios-Token": token } : {}),
        ...(body ? { "Content-Type": "application/json" } : {}),
      },
      ...(body ? { body: JSON.stringify(body) } : {}),
    });
    if (!response.ok) {
      const error = await response.json().catch(() => ({}));
      if (response.status === 401 || response.status === 403) {
        throw new Error(
          "Review authorization expired or was rejected. Reopen this curio from nth for a fresh link.",
        );
      }
      throw new Error(error.message || `Request failed (${response.status}).`);
    }
    return response.status === 204 ? null : response.json();
  }
  function draftSlot() {
    if (selected) return `reply:${selected}`;
    if (target) return `new:${revision}:${target.element_id}`;
    return null;
  }
  function editing() {
    return Boolean($("comment-body").value.trim());
  }
  function currentThread() {
    return state?.threads.find((thread) => thread.id === selected);
  }
  function pending() {
    return state.threads
      .flatMap((thread) => thread.messages)
      .filter(
        (message) => message.author === "human" && message.delivery === "saved",
      );
  }
  // Sent and not answered yet: an agent reply answers what came before it.
  function waiting(thread) {
    const last = thread.messages.at(-1);
    return (
      last?.author === "human" &&
      (last.delivery === "queued" || last.delivery === "recorded")
    );
  }
  // Waiting with a session running to answer it; with none, the feedback
  // waits for the session to run again.
  function working(thread) {
    return waiting(thread) && state.listening;
  }
  async function refresh() {
    if (refreshing) {
      refreshAgain = true;
      return;
    }
    refreshing = true;
    try {
      const snapshot = await api(
        `/api/state${revision ? `?revision=${encodeURIComponent(revision)}` : ""}`,
      );
      state = snapshot;
      revision = snapshot.revision.number;
      const url = new URL(location.href);
      url.searchParams.set("revision", String(revision));
      history.replaceState(null, "", url.pathname + url.search);
      render();
      if (loadedRevision !== revision) loadDocument();
    } catch (error) {
      say(error.message, true);
    } finally {
      refreshing = false;
      if (refreshAgain) {
        refreshAgain = false;
        refresh();
      }
    }
  }
  function render() {
    document.title = `${state.curio.title} · Curios`;
    $("title").textContent = state.curio.title;
    const picker = $("revision");
    if (document.activeElement !== picker) {
      picker.replaceChildren(
        ...state.revisions.map(
          (item) =>
            new Option(
              `r${item.number} · ${item.title || "Revision"}`,
              String(item.number),
            ),
        ),
      );
      picker.value = String(revision);
    }
    picker.disabled = busy;
    $("update").hidden = state.curio.current_revision === revision;
    $("update").textContent = `View latest · r${state.curio.current_revision}`;
    if (selected && !currentThread()) selected = null;
    renderPopover();
    schedulePins();
  }
  function renderPopover() {
    const thread = currentThread();
    $("popover").hidden = !thread && !target;
    $("submit").disabled = busy;
    $("submit").textContent = thread ? "Reply" : "Comment";
    $("resolve").hidden = !thread;
    const messages = $("messages");
    $("working").hidden = !thread || !waiting(thread);
    $("working").querySelector(".spinner").hidden = !state.listening;
    $("working-text").textContent = state.listening
      ? "nth is working…"
      : "Waiting for nth: its session is not running.";
    if (!thread) {
      messages.replaceChildren();
      delete messages.dataset.signature;
      return;
    }
    $("resolve").textContent =
      thread.status === "resolved" ? "Reopen" : "Resolve";
    // Only the conversation itself: who said what.
    const signature = JSON.stringify(
      thread.messages.map((message) => [message.author, message.body]),
    );
    if (messages.dataset.signature === signature) return;
    messages.dataset.signature = signature;
    messages.replaceChildren(
      ...thread.messages.map((message) => {
        const item = document.createElement("li"),
          author = document.createElement("strong");
        item.className = message.author;
        author.textContent = message.author === "agent" ? "nth" : "You";
        item.append(author, document.createTextNode(message.body));
        return item;
      }),
    );
  }
  function openPopover(next) {
    if (busy) return false;
    if (editing()) {
      say("Send or clear your comment first.");
      $("comment-body").focus();
      return false;
    }
    selected = next.thread || null;
    target = next.target || null;
    $("comment-body").value = readDraft(draftSlot()) || "";
    say(HINT);
    render();
    $("comment-body").focus({ preventScroll: true });
    return true;
  }
  function closePopover() {
    if (busy) return;
    selected = null;
    target = null;
    $("comment-body").value = "";
    render();
  }
  function frameDocument() {
    try {
      return frame.contentDocument;
    } catch {
      return null;
    }
  }
  function anchorElement(id) {
    const doc = frameDocument();
    if (!doc || !id) return null;
    const matches = Array.from(doc.querySelectorAll("[id]")).filter(
      (element) => element.id === id,
    );
    return matches.length === 1 ? matches[0] : null;
  }
  // The nearest element with an ID worth commenting on, if any.
  function eligible(element) {
    const ids = new Set(state.revision.anchors.map((anchor) => anchor.id));
    for (let node = element; node?.nodeType === 1; node = node.parentElement)
      if (ids.has(node.id) && anchorElement(node.id) === node) return node;
    return null;
  }
  function makeTarget(element) {
    const label = state.revision.anchors.find(
      (anchor) => anchor.id === element.id,
    )?.label;
    return {
      element_id: element.id,
      kind: "element",
      label: label || element.id,
      quote: (element.textContent || "")
        .trim()
        .replace(/\s+/g, " ")
        .slice(0, 180),
    };
  }
  function loadDocument() {
    frameCleanup();
    loadedRevision = revision;
    // Do not trust a server-provided URL to navigate the privileged frame elsewhere.
    frame.src = `/content/${encodeURIComponent(revision)}`;
  }
  frame.addEventListener("load", () => {
    frameCleanup();
    if (!state || !loadedRevision) return;
    const doc = frameDocument();
    if (!doc) {
      say("The document could not be opened for review.", true);
      return;
    }
    const abort = new AbortController(),
      options = { capture: true, signal: abort.signal };
    const listen = (node, event, handler) =>
      node.addEventListener(event, handler, options);
    listen(doc, "click", (event) => {
      event.preventDefault();
      event.stopPropagation();
      const link = event.target.closest?.("a[href]");
      if (link) {
        let url;
        try {
          url = new URL(
            link.getAttribute("href"),
            frame.contentWindow.location.href,
          );
        } catch {
          return;
        }
        if (
          url.origin === location.origin &&
          url.pathname === `/content/${revision}` &&
          url.hash
        ) {
          try {
            doc
              .getElementById(decodeURIComponent(url.hash.slice(1)))
              ?.scrollIntoView();
          } catch {
            /* Invalid fragment. */
          }
        } else if (
          ["https:", "http:"].includes(url.protocol) &&
          url.origin !== location.origin
        ) {
          if (window.confirm(`Open this link outside Curios?\n${url.href}`))
            window.open(url.href, "_blank", "noopener,noreferrer");
        } else
          say(
            "Document navigation is blocked. Use the revision selector to change snapshots.",
          );
        return;
      }
      const element = eligible(event.target);
      if (element) openPopover({ target: makeTarget(element) });
      else if (!editing()) closePopover();
    });
    listen(doc, "auxclick", (event) => event.preventDefault());
    listen(doc, "submit", (event) => event.preventDefault());
    listen(doc, "dragstart", (event) => event.preventDefault());
    listen(doc, "keydown", (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        closePopover();
      }
      if (event.key === "Enter" && event.target.closest?.("a[href]")) {
        event.preventDefault();
        event.target.click();
      }
    });
    listen(doc, "scroll", schedulePins);
    listen(doc, "load", schedulePins);
    const resize = new ResizeObserver(schedulePins);
    resize.observe(doc.documentElement);
    frameCleanup = () => {
      abort.abort();
      resize.disconnect();
    };
    schedulePins();
  });
  function schedulePins() {
    if (!pinFrame) pinFrame = requestAnimationFrame(drawPins);
  }
  function viewRect(nextTarget) {
    const element = anchorElement(nextTarget.element_id);
    if (!element) return null;
    const box = element.getBoundingClientRect(),
      r = nextTarget.rect || { x: 0, y: 0, width: 1, height: 1 };
    return visibleRect(element, {
      left: box.left + box.width * r.x,
      top: box.top + box.height * r.y,
      width: box.width * r.width,
      height: box.height * r.height,
    });
  }
  function visibleRect(element, box) {
    let left = box.left,
      top = box.top;
    let right = left + box.width,
      bottom = top + box.height;
    const doc = frameDocument();
    const clips = (value) => /^(auto|scroll|hidden|clip)$/.test(value);
    const styleOf = (node) => doc.defaultView.getComputedStyle(node);
    function containingBlock(node, fixed) {
      for (
        let parent = node.parentElement;
        parent;
        parent = parent.parentElement
      ) {
        const style = styleOf(parent);
        if (
          (!fixed && style.position !== "static") ||
          style.transform !== "none" ||
          style.perspective !== "none" ||
          style.filter !== "none" ||
          style.backdropFilter !== "none" ||
          /\b(layout|paint|strict|content)\b/.test(style.contain) ||
          /\b(transform|perspective|filter)\b/.test(style.willChange) ||
          style.contentVisibility === "auto"
        )
          return parent;
      }
      return null;
    }
    // Overflow between an out-of-flow box and its containing block does not
    // clip it. Repeat at each positioned ancestor (including fixed subtrees).
    for (
      let node = element, ancestor;
      (ancestor = /^(fixed|absolute)$/.test(styleOf(node).position)
        ? containingBlock(node, styleOf(node).position === "fixed")
        : node.parentElement);
      node = ancestor
    ) {
      const style = styleOf(ancestor);
      if (!clips(style.overflowX) && !clips(style.overflowY)) continue;
      const bounds = ancestor.getBoundingClientRect();
      // Client dimensions exclude borders and scrollbars; bounding dimensions
      // carry CSS zoom and axis-aligned transform scaling into frame pixels.
      const sx = ancestor.offsetWidth ? bounds.width / ancestor.offsetWidth : 1;
      const sy = ancestor.offsetHeight
        ? bounds.height / ancestor.offsetHeight
        : 1;
      const x = bounds.left + ancestor.clientLeft * sx;
      const y = bounds.top + ancestor.clientTop * sy;
      if (clips(style.overflowX)) {
        left = Math.max(left, x);
        right = Math.min(right, x + ancestor.clientWidth * sx);
      }
      if (clips(style.overflowY)) {
        top = Math.max(top, y);
        bottom = Math.min(bottom, y + ancestor.clientHeight * sy);
      }
    }
    left = Math.max(0, left);
    top = Math.max(0, top);
    right = Math.min(doc.defaultView.innerWidth, right);
    bottom = Math.min(doc.defaultView.innerHeight, bottom);
    if (right <= left || bottom <= top) return null;
    const bounds = frame.getBoundingClientRect(),
      overlay = $("overlays");
    const surface = overlay.getBoundingClientRect();
    const fx = bounds.width / frame.offsetWidth,
      fy = bounds.height / frame.offsetHeight;
    const ox = surface.width / overlay.clientWidth,
      oy = surface.height / overlay.clientHeight;
    const originX = bounds.left + frame.clientLeft * fx;
    const originY = bounds.top + frame.clientTop * fy;
    left = Math.max(originX + left * fx, surface.left, 0);
    top = Math.max(originY + top * fy, surface.top, 0);
    right = Math.min(originX + right * fx, surface.right, window.innerWidth);
    bottom = Math.min(
      originY + bottom * fy,
      surface.bottom,
      window.innerHeight,
    );
    if (right <= left || bottom <= top || !ox || !oy) return null;
    return {
      left: (left - surface.left) / ox,
      top: (top - surface.top) / oy,
      width: (right - left) / ox,
      height: (bottom - top) / oy,
    };
  }
  function drawPins() {
    pinFrame = 0;
    if (!state || !frameDocument()) return;
    const pins = $("pins"),
      existing = new Map(
        Array.from(pins.children).map((node) => [node.dataset.id, node]),
      );
    let anchor = null;
    for (const thread of state.threads) {
      if (
        thread.attachment === "detached" &&
        thread.created_revision !== revision
      )
        continue;
      const box = viewRect(thread.target);
      if (!box || !box.width || !box.height) continue;
      const x = box.left + Math.min(box.width / 2, 10),
        y = box.top + Math.min(box.height / 2, 10);
      let pin = existing.get(thread.id);
      if (!pin) {
        pin = document.createElement("button");
        pin.className = "pin";
        pin.dataset.id = thread.id;
        pin.addEventListener("click", () => {
          if (selected === thread.id) closePopover();
          else openPopover({ thread: thread.id });
        });
        pins.append(pin);
      }
      pin.textContent = String(thread.number);
      pin.classList.toggle("working", working(thread));
      pin.classList.toggle("resolved", thread.status === "resolved");
      pin.setAttribute(
        "aria-label",
        `Thread ${thread.number}: ${thread.target.label || "Document"}, ${working(thread) ? "nth is working" : waiting(thread) ? "waiting for nth" : thread.status}`,
      );
      pin.setAttribute("aria-pressed", String(thread.id === selected));
      pin.style.left = `${x}px`;
      pin.style.top = `${y}px`;
      if (thread.id === selected)
        anchor = { left: x - 11, top: y - 11, width: 22, height: 22 };
      existing.delete(thread.id);
    }
    for (const pin of existing.values()) pin.remove();
    const box = target ? viewRect(target) : null;
    const overlay = $("selection-box");
    overlay.hidden = !box;
    if (box)
      for (const key of ["left", "top", "width", "height"])
        overlay.style[key] = `${box[key]}px`;
    placePopover(anchor || box);
  }
  // Below what it is about, or above when that leaves no room, and always
  // inside the stage.
  function placePopover(box) {
    const popover = $("popover");
    if (popover.hidden || !box) return;
    const stage = $("overlays"),
      gap = 8;
    const width = popover.offsetWidth,
      height = popover.offsetHeight;
    let top = box.top + box.height + gap;
    if (top + height > stage.clientHeight && box.top - gap - height >= 0)
      top = box.top - gap - height;
    top = Math.max(gap, Math.min(top, stage.clientHeight - height - gap));
    const left = Math.max(
      gap,
      Math.min(box.left, stage.clientWidth - width - gap),
    );
    popover.style.left = `${left}px`;
    popover.style.top = `${top}px`;
  }
  async function mutate(steps) {
    if (busy) return false;
    busy = true;
    render();
    $("comment-body").readOnly = true;
    try {
      await steps();
      return true;
    } catch (error) {
      say(error.message, true);
      return false;
    } finally {
      busy = false;
      $("comment-body").readOnly = false;
      await refresh();
    }
  }
  // Everything saved goes to nth at once, so a send that failed before
  // goes along with the next comment.
  // `saved` is the thread just written, newer than `state` may be.
  async function sendPending(saved) {
    const ids = new Set(pending().map((message) => message.id));
    for (const message of saved.messages)
      if (message.author === "human" && message.delivery === "saved")
        ids.add(message.id);
    const payload = { message_ids: [...ids] };
    if (!payload.message_ids.length) return;
    await api("/api/send", {
      request_id: requestId("send", payload),
      ...payload,
    });
    requests.delete("send");
  }
  async function changeRevision(number) {
    if (busy || editing()) {
      $("revision").value = String(revision);
      say("Send or clear your comment before changing revisions.");
      return;
    }
    closePopover();
    revision = Number(number);
    await refresh();
  }
  $("comment-body").addEventListener("input", () => {
    const slot = draftSlot();
    if (slot) writeDraft(slot, $("comment-body").value || null);
  });
  $("comment-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const body = $("comment-body").value.trim();
    const slot = draftSlot();
    if (!body || !slot) return;
    const thread = currentThread();
    await mutate(async () => {
      let saved;
      if (thread) {
        const payload = { expected_version: thread.version, revision, body };
        saved = await api(
          `/api/threads/${encodeURIComponent(thread.id)}/messages`,
          { request_id: requestId(slot, payload), ...payload },
        );
      } else {
        const payload = { revision, target, body };
        saved = await api("/api/comments", {
          request_id: requestId(slot, payload),
          ...payload,
        });
        target = null;
        selected = saved.id;
      }
      requests.delete(slot);
      writeDraft(slot, null);
      $("comment-body").value = "";
      await sendPending(saved);
      say(HINT);
    });
  });
  $("resolve").addEventListener("click", async () => {
    const thread = currentThread();
    if (!thread) return;
    const status = thread.status === "resolved" ? "open" : "resolved";
    await mutate(() =>
      api(`/api/threads/${encodeURIComponent(thread.id)}/status`, {
        expected_version: thread.version,
        status,
      }),
    );
  });
  $("close").addEventListener("click", closePopover);
  $("revision").addEventListener("change", (event) =>
    changeRevision(event.target.value),
  );
  $("update").addEventListener("click", () =>
    changeRevision(state.curio.current_revision),
  );
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      closePopover();
    }
    // Enter sends, as in a chat; Shift+Enter is a new line.
    if (
      event.key === "Enter" &&
      !event.shiftKey &&
      !event.isComposing &&
      event.target.tagName === "TEXTAREA"
    ) {
      event.preventDefault();
      if (!busy) event.target.form.requestSubmit();
    }
  });
  window.addEventListener("resize", schedulePins);
  document.addEventListener("scroll", schedulePins, true);
  // A bounded authoritative poll also reconciles CLI writes and SSE reconnects.
  setInterval(() => {
    if (authenticated && !document.hidden && !busy) refresh();
  }, 4000);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && authenticated) refresh();
  });
  async function events() {
    try {
      const response = await fetch("/api/events", {
        headers: {
          "X-Curios-Request": "viewer",
          ...(token ? { "X-Curios-Token": token } : {}),
        },
        credentials: "same-origin",
        cache: "no-store",
      });
      if (
        !response.ok ||
        !response.body ||
        !response.headers.get("content-type")?.includes("text/event-stream")
      )
        return;
      const reader = response.body.getReader(),
        decoder = new TextDecoder();
      let buffer = "";
      while (true) {
        const result = await reader.read();
        if (result.done) break;
        buffer += decoder
          .decode(result.value, { stream: true })
          .replace(/\r\n/g, "\n");
        let boundary;
        while ((boundary = buffer.indexOf("\n\n")) >= 0) {
          const event = buffer.slice(0, boundary);
          buffer = buffer.slice(boundary + 2);
          if (/^(data:|event:)/m.test(event)) refresh();
        }
        if (buffer.length > 65536) {
          await reader.cancel();
          break;
        }
      }
    } catch {
      /* Snapshot polling remains available when streaming is unavailable. */
    } finally {
      if (authenticated) setTimeout(events, 5000);
    }
  }
  async function start() {
    try {
      if (token) {
        // Clear the URL promptly, retaining the capability only in memory
        // until the host establishes its HttpOnly reload credential.
        await api("/api/handoff", {});
        token = "";
      }
      authenticated = true;
      await refresh();
      if (state) {
        say(HINT);
        events();
      } else {
        authenticated = false;
        say(
          "Could not open this review. Retrying; if authorization has expired, reopen the curio from nth.",
          true,
        );
        setTimeout(start, 5000);
      }
    } catch (error) {
      say(`${error.message} Retrying the review handoff…`, true);
      setTimeout(start, 5000);
    }
  }
  start();
})();
