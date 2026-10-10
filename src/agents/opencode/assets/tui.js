// herdr-respan: pane-local V1/V2 ownership, including attached shared servers.
import { collector, enabled } from "./herdr-respan/bridge.js";

function ownership(route, getSession) {
  let selected;
  function descendant(id, boundary) {
    const seen = new Set();
    while (id && !seen.has(id)) {
      if (id === boundary) return true;
      seen.add(id);
      id = getSession(id)?.parentID;
    }
    return false;
  }
  return (id) => {
    const current = route();
    if (!current) { selected = undefined; return false; }
    if (!selected || !descendant(current, selected)) selected = current;
    return descendant(id, selected);
  };
}

function tui(api) {
  if (!enabled()) return;
  const owns = ownership(() => api.route.current?.name === "session" ? api.route.current.params?.sessionID : undefined,
    (id) => api.state.session.get(id));
  const capture = collector({ owns, cwd: process.cwd() });
  const types = ["session.created", "session.updated", "session.deleted", "session.status", "session.idle", "session.error",
    "message.updated", "message.part.updated", "message.part.removed"];
  const unsubscribe = types.map((type) => api.event.on(type, (event) => {
    try { capture.v1(event); } catch {}
  }));
  api.lifecycle.onDispose(() => { for (const stop of unsubscribe) stop(); });
}

function setup(api) {
  if (!enabled()) return;
  const owns = ownership(() => {
    const route = api.ui.router.current();
    return route.type === "session" ? route.sessionID : undefined;
  }, (id) => api.data.session.get(id));
  const capture = collector({ owns, cwd: process.cwd() });
  return api.data.listen(({ details }) => { try { capture.v2(details); } catch {} });
}

export default { id: "herdr-respan.opencode.tui", tui, setup };
