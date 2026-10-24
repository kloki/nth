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
  let state, loadedRevision, selected, target, drag;
  let mode = "element",
    busy = false,
    authenticated = false,
    refreshing = false,
    refreshAgain = false;
  let frameCleanup = () => {},
    pinFrame = 0;
  const frame = $("document");
  const drafts = new Map();
  const requests = new Map();
  const clamp = (n, lo = 0, hi = 1) => Math.max(lo, Math.min(hi, n));
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
      return JSON.parse(sessionStorage.getItem(draftKey(key)) || "null");
    } catch {
      return null;
    }
  }
  function writeDraft(key, value) {
    drafts.set(key, value);
    try {
      if (value) sessionStorage.setItem(draftKey(key), JSON.stringify(value));
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
  function editing() {
    return Boolean(target || drag || $("reply-body").value.trim());
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
    $("send").textContent = `Send to nth (${pending().length})`;
    $("send").disabled = busy || !pending().length;
    $("thread-count").textContent = `${state.threads.length} threads`;
    $("empty").hidden = Boolean(state.threads.length || target);
    const anchorPicker = $("anchor"),
      oldAnchor = anchorPicker.value;
    if (document.activeElement !== anchorPicker) {
      anchorPicker.replaceChildren(
        new Option("Whole document", ""),
        ...state.revision.anchors.map(
          (anchor) => new Option(anchor.label || anchor.id, anchor.id),
        ),
      );
      anchorPicker.value = oldAnchor;
    }
    const list = $("thread-list");
    // Keep focused buttons alive across polling, as well as composer text and scroll.
    const existing = new Map(
      Array.from(list.children).map((node) => [node.dataset.id, node]),
    );
    for (const thread of state.threads) {
      let button = existing.get(thread.id);
      if (!button) {
        button = document.createElement("button");
        button.className = "thread-card";
        button.dataset.id = thread.id;
        button.append(
          document.createElement("strong"),
          document.createElement("span"),
        );
        button.addEventListener("click", () => selectThread(thread.id, true));
      }
      button.setAttribute("aria-current", String(thread.id === selected));
      button.firstChild.textContent = `#${thread.number} · ${thread.target.label || "Document"}`;
      button.lastChild.textContent = `${thread.status} · ${thread.attachment}${thread.messages.some((m) => m.delivery === "saved") ? " · not sent" : ""}`;
      if (button.parentNode !== list) list.append(button);
      existing.delete(thread.id);
    }
    for (const button of existing.values()) button.remove();
    renderThread();
    schedulePins();
  }
  function renderThread() {
    const thread = currentThread();
    $("thread-detail").hidden = !thread;
    if (!thread) return;
    $("thread-title").textContent = `Thread #${thread.number}`;
    $("thread-context").textContent = [thread.target.label, thread.target.quote]
      .filter(Boolean)
      .join(" — ");
    $("thread-state").textContent =
      `${thread.status} · ${thread.attachment}${thread.attachment === "detached" ? " — original context retained; no pin on this revision" : ""}`;
    $("original").hidden = thread.created_revision === revision;
    $("resolve").textContent =
      thread.status === "resolved" ? "Reopen thread" : "Resolve thread";
    const messages = $("messages");
    const signature = JSON.stringify(thread.messages);
    if (messages.dataset.signature === signature) return;
    messages.dataset.signature = signature;
    messages.replaceChildren(
      ...thread.messages.map((message) => {
        const item = document.createElement("li"),
          meta = document.createElement("p"),
          body = document.createElement("p");
        meta.className = "message-meta";
        meta.textContent = `${message.author === "agent" ? "nth" : "You"} · ${message.delivery} · ${message.created_at}`;
        body.className = "message-body";
        body.textContent = message.body;
        item.append(meta, body);
        return item;
      }),
    );
  }
  function selectThread(id, focus = false) {
    if (busy) return;
    if (selected)
      writeDraft(`reply:${selected}`, $("reply-body").value || null);
    selected = id;
    $("reply-body").value = readDraft(`reply:${id}`) || "";
    render();
    if (focus) {
      $("thread-detail").scrollIntoView({ block: "nearest" });
      $("reply-body").focus({ preventScroll: true });
    }
    schedulePins();
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
    if (!doc) return null;
    if (!id) return doc.documentElement;
    const matches = Array.from(doc.querySelectorAll("[id]")).filter(
      (element) => element.id === id,
    );
    return matches.length === 1 ? matches[0] : null;
  }
  function eligible(element) {
    const ids = new Set(state.revision.anchors.map((anchor) => anchor.id));
    for (let node = element; node?.nodeType === 1; node = node.parentElement)
      if (ids.has(node.id) && anchorElement(node.id) === node) return node;
    return frameDocument()?.documentElement;
  }
  function makeTarget(element, kind = "element") {
    const meaningful =
      element !== frameDocument()?.documentElement && element?.id;
    const label = state.revision.anchors.find(
      (anchor) => anchor.id === element?.id,
    )?.label;
    return {
      element_id: meaningful ? element.id : null,
      kind: meaningful ? kind : kind === "region" ? "region" : "document",
      label: label || (meaningful ? element.id : "Whole document"),
      quote: (element?.textContent || "")
        .trim()
        .replace(/\s+/g, " ")
        .slice(0, 180),
    };
  }
  function persistComposer() {
    if (target)
      writeDraft(`new:${revision}`, { target, body: $("comment-body").value });
  }
  function beginComment(nextTarget, focus = true) {
    if (busy) return;
    if (target && $("comment-body").value.trim()) {
      say(
        "Save or cancel your current comment before choosing another target.",
      );
      return;
    }
    target = nextTarget;
    $("composer").hidden = false;
    $("target-label").textContent = `${target.kind} · ${target.label}`;
    $("region-fields").hidden = target.kind !== "region";
    $("region-fields").disabled = target.kind !== "region";
    if (target.rect)
      for (const key of ["x", "y", "width", "height"])
        $(`region-${key}`).value = String(target.rect[key]);
    persistComposer();
    schedulePins();
    $("empty").hidden = true;
    if (focus) {
      $("composer").scrollIntoView({ block: "nearest" });
      $("comment-body").focus({ preventScroll: true });
    }
  }
  function cancelComment() {
    target = null;
    drag = null;
    $("composer").hidden = true;
    $("region-fields").disabled = true;
    $("comment-body").value = "";
    writeDraft(`new:${revision}`, null);
    schedulePins();
    $("add-comment").focus();
  }
  function setMode(next) {
    mode = next;
    $("element-mode").setAttribute("aria-pressed", String(mode === "element"));
    $("region-mode").setAttribute("aria-pressed", String(mode === "region"));
    $("selection-help").textContent =
      mode === "region"
        ? "Drag a region in the document, or choose a target and Add comment to enter bounds with the keyboard."
        : "Click a document element, or choose a target and Add comment. Escape cancels selection.";
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
      if (mode === "element") beginComment(makeTarget(eligible(event.target)));
    });
    listen(doc, "auxclick", (event) => event.preventDefault());
    listen(doc, "submit", (event) => event.preventDefault());
    listen(doc, "dragstart", (event) => event.preventDefault());
    listen(doc, "keydown", (event) => {
      if (event.key === "Escape") {
        event.preventDefault();
        escape();
      }
      if (event.key === "Enter" && event.target.closest?.("a[href]")) {
        event.preventDefault();
        event.target.click();
      }
    });
    listen(doc, "pointerdown", (event) => {
      if (mode !== "region" || event.button !== 0 || target) return;
      event.preventDefault();
      const element = eligible(event.target),
        bounds = element.getBoundingClientRect();
      if (!bounds.width || !bounds.height) return;
      drag = {
        element,
        startX: event.clientX,
        startY: event.clientY,
        x: event.clientX,
        y: event.clientY,
      };
      try {
        event.target.setPointerCapture(event.pointerId);
      } catch {
        /* Document listeners still capture in-frame drags. */
      }
      schedulePins();
    });
    listen(doc, "pointermove", (event) => {
      if (drag) {
        drag.x = event.clientX;
        drag.y = event.clientY;
        schedulePins();
      }
    });
    listen(doc, "pointerup", (event) => {
      if (!drag) return;
      drag.x = event.clientX;
      drag.y = event.clientY;
      const bounds = drag.element.getBoundingClientRect();
      const x = clamp(
        (Math.min(drag.startX, drag.x) - bounds.left) / bounds.width,
      );
      const y = clamp(
        (Math.min(drag.startY, drag.y) - bounds.top) / bounds.height,
      );
      const right = clamp(
        (Math.max(drag.startX, drag.x) - bounds.left) / bounds.width,
      );
      const bottom = clamp(
        (Math.max(drag.startY, drag.y) - bounds.top) / bounds.height,
      );
      const next = makeTarget(drag.element, "region");
      drag = null;
      if (right - x < 0.001 || bottom - y < 0.001) {
        schedulePins();
        return;
      }
      next.rect = { x, y, width: right - x, height: bottom - y };
      beginComment(next);
    });
    listen(doc, "pointercancel", () => {
      drag = null;
      schedulePins();
    });
    listen(doc, "scroll", schedulePins);
    listen(doc, "load", schedulePins);
    const resize = new ResizeObserver(schedulePins);
    resize.observe(doc.documentElement);
    frameCleanup = () => {
      abort.abort();
      resize.disconnect();
    };
    const draft = readDraft(`new:${revision}`);
    if (draft?.target && !target) {
      $("comment-body").value = draft.body || "";
      beginComment(draft.target, false);
    }
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
    for (
      let ancestor = element.parentElement;
      ancestor;
      ancestor = ancestor.parentElement
    ) {
      const style = doc.defaultView.getComputedStyle(ancestor);
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
    for (const thread of state.threads) {
      if (
        thread.attachment === "detached" &&
        thread.created_revision !== revision
      )
        continue;
      const box = viewRect(thread.target);
      if (!box || !box.width || !box.height) continue;
      const x = box.left + Math.min(box.width / 2, 16),
        y = box.top + Math.min(box.height / 2, 16);
      let pin = existing.get(thread.id);
      if (!pin) {
        pin = document.createElement("button");
        pin.className = "pin";
        pin.dataset.id = thread.id;
        pin.addEventListener("click", () => selectThread(thread.id, true));
        pins.append(pin);
      }
      pin.textContent = String(thread.number);
      pin.setAttribute(
        "aria-label",
        `Thread ${thread.number}: ${thread.target.label || "Document"}, ${thread.status}`,
      );
      pin.setAttribute("aria-pressed", String(thread.id === selected));
      pin.style.left = `${x}px`;
      pin.style.top = `${y}px`;
      const halfWidth = pin.offsetWidth / 2,
        halfHeight = pin.offsetHeight / 2;
      pin.style.clipPath = `inset(${Math.max(0, box.top - y + halfHeight)}px ${Math.max(0, x + halfWidth - box.left - box.width)}px ${Math.max(0, y + halfHeight - box.top - box.height)}px ${Math.max(0, box.left - x + halfWidth)}px)`;
      existing.delete(thread.id);
    }
    for (const pin of existing.values()) pin.remove();
    const box = drag
      ? visibleRect(drag.element, {
          left: Math.min(drag.startX, drag.x),
          top: Math.min(drag.startY, drag.y),
          width: Math.abs(drag.x - drag.startX),
          height: Math.abs(drag.y - drag.startY),
        })
      : target
        ? viewRect(target)
        : null;
    const overlay = $("selection-box");
    overlay.hidden = !box;
    if (box)
      for (const key of ["left", "top", "width", "height"])
        overlay.style[key] = `${box[key]}px`;
  }
  async function mutate(path, payload, success) {
    if (busy) return false;
    busy = true;
    render();
    document.querySelectorAll("form button").forEach((button) => {
      button.disabled = true;
    });
    document
      .querySelectorAll("textarea, #region-fields input")
      .forEach((input) => {
        input.readOnly = true;
      });
    try {
      await api(path, payload);
      await success?.();
      await refresh();
      return true;
    } catch (error) {
      say(error.message, true);
      await refresh();
      return false;
    } finally {
      busy = false;
      document.querySelectorAll("form button").forEach((button) => {
        button.disabled = false;
      });
      document
        .querySelectorAll("textarea, #region-fields input")
        .forEach((input) => {
          input.readOnly = false;
        });
      render();
    }
  }
  async function changeRevision(number) {
    if (busy || editing()) {
      $("revision").value = String(revision);
      say(
        "Save or cancel the current comment or reply before changing revisions.",
      );
      return;
    }
    selected = null;
    $("reply-body").value = "";
    revision = Number(number);
    await refresh();
  }
  $("comment-body").addEventListener("input", persistComposer);
  $("reply-body").addEventListener("input", () => {
    if (selected)
      writeDraft(`reply:${selected}`, $("reply-body").value || null);
  });
  $("comment-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const body = $("comment-body").value.trim();
    if (!body || !target) {
      say("Write a comment before saving.");
      return;
    }
    if (target.kind === "region") {
      const rect = readRegion();
      if (!rect) {
        say("Region bounds must fit within 0–1.", true);
        return;
      }
      target.rect = rect;
    }
    const payload = { revision, target, body };
    const ok = await mutate(
      "/api/comments",
      { request_id: requestId("comment", payload), ...payload },
      () => {
        cancelComment();
        requests.delete("comment");
        say("Comment saved. It will reach nth only when you Send.");
      },
    );
    if (ok) schedulePins();
  });
  $("reply-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const thread = currentThread(),
      body = $("reply-body").value.trim();
    if (!body || !thread) return;
    const payload = { expected_version: thread.version, body };
    await mutate(
      `/api/threads/${encodeURIComponent(thread.id)}/messages`,
      { request_id: requestId(`reply:${thread.id}`, payload), ...payload },
      () => {
        $("reply-body").value = "";
        writeDraft(`reply:${thread.id}`, null);
        requests.delete(`reply:${thread.id}`);
        say("Reply saved. Send to nth when ready.");
      },
    );
  });
  $("send").addEventListener("click", async () => {
    const payload = { message_ids: pending().map((message) => message.id) };
    if (!payload.message_ids.length) return;
    await mutate(
      "/api/send",
      { request_id: requestId("send", payload), ...payload },
      () => {
        requests.delete("send");
        say(
          "Feedback queued for its owning nth session. Queued does not mean answered.",
        );
      },
    );
  });
  $("resolve").addEventListener("click", async () => {
    const thread = currentThread();
    if (!thread) return;
    const status = thread.status === "resolved" ? "open" : "resolved";
    await mutate(
      `/api/threads/${encodeURIComponent(thread.id)}/status`,
      { expected_version: thread.version, status },
      () =>
        say(status === "resolved" ? "Thread resolved." : "Thread reopened."),
    );
  });
  function readRegion() {
    const tolerance = 1e-9;
    const rect = Object.fromEntries(
      ["x", "y", "width", "height"].map((key) => [
        key,
        $(`region-${key}`).value === ""
          ? NaN
          : Number($(`region-${key}`).value),
      ]),
    );
    if (
      Object.values(rect).some((n) => !Number.isFinite(n)) ||
      rect.width <= 0 ||
      rect.height <= 0 ||
      rect.x < -tolerance ||
      rect.y < -tolerance ||
      rect.x > 1 + tolerance ||
      rect.y > 1 + tolerance ||
      rect.x + rect.width > 1 + tolerance ||
      rect.y + rect.height > 1 + tolerance
    ) {
      return null;
    }
    rect.x = clamp(rect.x);
    rect.y = clamp(rect.y);
    rect.width = Math.min(rect.width, 1 - rect.x);
    rect.height = Math.min(rect.height, 1 - rect.y);
    return rect.width > 0 && rect.height > 0 ? rect : null;
  }
  $("region-fields").addEventListener("input", () => {
    if (!target || target.kind !== "region") return;
    const rect = readRegion();
    if (!rect) {
      say("Region bounds must fit within 0–1.", true);
      return;
    }
    target.rect = rect;
    persistComposer();
    schedulePins();
  });
  $("add-comment").addEventListener("click", () => {
    if (!state) return;
    const next = makeTarget(anchorElement($("anchor").value), mode);
    if (mode === "region") next.rect = { x: 0, y: 0, width: 1, height: 1 };
    beginComment(next);
  });
  $("element-mode").addEventListener("click", () => setMode("element"));
  $("region-mode").addEventListener("click", () => setMode("region"));
  $("cancel-comment").addEventListener("click", cancelComment);
  $("revision").addEventListener("change", (event) =>
    changeRevision(event.target.value),
  );
  $("update").addEventListener("click", () =>
    changeRevision(state.curio.current_revision),
  );
  $("original").addEventListener("click", () =>
    changeRevision(currentThread().created_revision),
  );
  $("close-thread").addEventListener("click", () => {
    if (!state || busy) return;
    if (selected)
      writeDraft(`reply:${selected}`, $("reply-body").value || null);
    selected = null;
    $("reply-body").value = "";
    render();
    $("add-comment").focus();
  });
  function escape() {
    if (busy || !state) return;
    if (drag) {
      drag = null;
      schedulePins();
      $("region-mode").focus();
    } else if (target) {
      persistComposer();
      target = null;
      $("composer").hidden = true;
      $("region-fields").disabled = true;
      schedulePins();
      $("add-comment").focus();
      say("Composer closed. Draft retained for recovery.");
    } else $("close-thread").click();
  }
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      escape();
    }
    if (
      (event.ctrlKey || event.metaKey) &&
      event.key === "Enter" &&
      event.target.tagName === "TEXTAREA"
    ) {
      event.preventDefault();
      if (!busy) event.target.form.requestSubmit();
    }
  });
  window.addEventListener("resize", schedulePins);
  document.addEventListener("scroll", schedulePins, true);
  window.addEventListener("beforeunload", () => {
    if (target) persistComposer();
  });
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
        say("Choose an element to comment. Save locally, then Send to nth.");
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
