// Dependency-free CDP probe (Node 22+):
// node crates/nth-curios/tests/browser_network.mjs "$(agent-browser get cdp-url)"
// Prints credential presence/attributes only, never capability or cookie values.
const socket = new WebSocket(process.argv[2]);
await new Promise((resolve) => socket.addEventListener("open", resolve));
let serial = 0;
const pending = new Map();
const paths = new Map();
const extra = new Map();
socket.addEventListener("message", ({ data }) => {
  const message = JSON.parse(data);
  if (message.id) {
    const waiter = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) waiter.reject(new Error(JSON.stringify(message.error)));
    else waiter.resolve(message.result);
  }
  if (message.method === "Network.requestWillBeSent") {
    paths.set(
      message.params.requestId,
      new URL(message.params.request.url).pathname,
    );
  }
  if (message.method === "Network.requestWillBeSentExtraInfo") {
    const p = message.params;
    extra.set(p.requestId, {
      cookieHeader: Object.keys(p.headers).some(
        (key) => key.toLowerCase() === "cookie",
      ),
      viewerHeader: Object.entries(p.headers).some(
        ([key, value]) =>
          key.toLowerCase() === "x-curios-request" && value === "viewer",
      ),
      cookies: p.associatedCookies.map(({ cookie, blockedReasons }) => ({
        path: cookie.path,
        httpOnly: cookie.httpOnly,
        sameSite: cookie.sameSite,
        blockedReasons,
      })),
    });
  }
});
function call(method, params = {}, sessionId) {
  return new Promise((resolve, reject) => {
    const id = ++serial;
    pending.set(id, { resolve, reject });
    socket.send(
      JSON.stringify({
        id,
        method,
        params,
        ...(sessionId ? { sessionId } : {}),
      }),
    );
  });
}
const { targetInfos } = await call("Target.getTargets");
const target = targetInfos.find(
  (target) =>
    target.type === "page" && target.url.startsWith("http://127.0.0.1:"),
);
const { sessionId } = await call("Target.attachToTarget", {
  targetId: target.targetId,
  flatten: true,
});
await call("Network.enable", {}, sessionId);
await call("Page.enable", {}, sessionId);
await call("Page.reload", {}, sessionId);
const deadline = Date.now() + 5000;
while (
  Date.now() < deadline &&
  ![...paths.values()].some((path) => path.startsWith("/content/"))
) {
  await new Promise((resolve) => setTimeout(resolve, 50));
}
await new Promise((resolve) => setTimeout(resolve, 200));
console.log(
  JSON.stringify(
    [...paths].map(([id, path]) => ({ path, ...extra.get(id) })),
    null,
    2,
  ),
);
socket.close();
