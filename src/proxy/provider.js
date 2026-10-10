// Ephemeral launch asset; no auth, user config, or persistent model selection edits.
import fs from "node:fs";
export default function (api) {
  const route = (_event, ctx) => {
    const model = ctx.model;
    if (!model || typeof model.baseUrl !== "string" || model.baseUrl.startsWith(__BASE__)) return;
    const protocol = model.api?.includes("anthropic") ? "anthropic"
      : model.api?.includes("openai") ? "openai"
      : model.api?.includes("google") || model.api?.includes("gemini") ? "gemini" : null;
    if (!protocol) throw new Error("herdr-respan proxy does not support model API " + model.api);
    const upstream = new URL(model.baseUrl);
    if (upstream.username || upstream.password || upstream.search || upstream.hash) {
      throw new Error("herdr-respan requires a credential-free provider base URL");
    }
    const provider = encodeURIComponent(model.provider);
    const registration = JSON.parse(fs.readFileSync(__REGISTRATION__, "utf8"));
    registration.upstream[protocol + "/" + provider] = model.baseUrl.replace(/\/$/, "");
    fs.writeFileSync(__REGISTRATION__ + ".tmp", JSON.stringify(registration), { mode: 0o600 });
    fs.renameSync(__REGISTRATION__ + ".tmp", __REGISTRATION__);
    const baseUrl = __BASE__ + "/" + protocol + "/__provider/" + provider;
    api.registerProvider(model.provider, { baseUrl });
    // ctx.model is the live model object. Update this invocation only; never call
    // setModel(), which can persist an unrelated model selection in user settings.
    model.baseUrl = baseUrl;
  };
  api.on("session_start", route);
  api.on("before_agent_start", route);
}
