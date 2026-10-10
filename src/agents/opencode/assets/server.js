// herdr-respan: V1 run/Mini only; shared-server ownership belongs to the pane TUI.
import { collector, enabled } from "../herdr-respan/bridge.js";

function localClient() {
  const args = process.argv.slice(2);
  const separator = args.indexOf("--");
  if (separator !== -1) args.splice(separator);
  if (args.some((arg) => arg === "--attach" || arg.startsWith("--attach="))) return false;
  while (args[0] === "--print-logs" || args[0] === "--log-level" || args[0]?.startsWith("--log-level=")) {
    args.splice(0, args[0] === "--log-level" ? 2 : 1);
  }
  return args[0] === "run" || (!["serve", "web", "attach"].includes(args[0]) && args.includes("--mini"));
}

async function server({ directory }) {
  if (!enabled() || !localClient()) return {};
  let selected;
  const children = new Set();
  const capture = collector({ cwd: directory, owns: (id) => id === selected });
  const safe = (fn) => async (...args) => { try { fn(...args); } catch {} };
  return {
    "chat.message": safe((input, output) => {
      if (!children.has(input.sessionID)) selected = input.sessionID;
      capture.prompt(input, output);
    }),
    config: safe(capture.configure),
    event: safe(({ event }) => {
      if (event.properties?.info?.parentID) children.add(event.properties.info.id);
      capture.v1(event);
    }),
  };
}

// Object entrypoints require V1 >= 1.18.29. V2 captures through its pane-local TUI.
export default { id: "herdr-respan.opencode", server, setup() {} };
