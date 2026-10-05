import type { Scope, ServerConfig, Transport } from "./types";

export type Kind = "command" | "url";

export interface FormValues {
  name: string;
  kind: Kind;
  enabled: boolean;
  command: string;
  args: string;
  env: string;
  cwd: string;
  url: string;
  transport: "" | Transport;
  headers: string;
  scope: Scope;
  idleTimeout: string;
  cacheLists: boolean;
}

export const emptyForm = (): FormValues => ({
  name: "",
  kind: "command",
  enabled: true,
  command: "",
  args: "",
  env: "",
  cwd: "",
  url: "",
  transport: "",
  headers: "",
  scope: "shared",
  idleTimeout: "",
  cacheLists: true,
});

const lines = (text: string) =>
  text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l !== "");

const joinPairs = (map: Record<string, string>, sep: string) =>
  Object.entries(map)
    .map(([k, v]) => `${k}${sep}${v}`)
    .join("\n");

function parsePairs(text: string, sep: string, what: string) {
  const out: Record<string, string> = {};
  for (const line of lines(text)) {
    const i = line.indexOf(sep);
    const key = i < 0 ? "" : line.slice(0, i).trim();
    if (!key) throw new Error(`${what}: expected "KEY${sep}VALUE", got "${line}"`);
    out[key] = line.slice(i + 1).trim();
  }
  return out;
}

export function toForm(name: string, s: ServerConfig): FormValues {
  return {
    name,
    kind: s.url !== null ? "url" : "command",
    enabled: s.enabled,
    command: s.command ?? "",
    args: s.args.join("\n"),
    env: joinPairs(s.env, "="),
    cwd: s.cwd ?? "",
    url: s.url ?? "",
    transport: s.transport ?? "",
    headers: joinPairs(s.headers, ": "),
    scope: s.scope,
    idleTimeout: s.idle_timeout === null ? "" : String(s.idle_timeout),
    cacheLists: s.cache_lists,
  };
}

/** Converts form input to a config entry; throws an Error with a user-facing message. */
export function fromForm(f: FormValues): { name: string; server: ServerConfig } {
  const name = f.name.trim();
  if (!name) throw new Error("Name is required");
  if (name.includes("/")) throw new Error('Name must not contain "/"');

  let idle: number | null = null;
  if (f.idleTimeout.trim() !== "") {
    idle = Number(f.idleTimeout);
    if (!Number.isInteger(idle) || idle < 0)
      throw new Error("Idle timeout must be a non-negative integer (seconds)");
  }

  const base = {
    enabled: f.enabled,
    scope: f.scope,
    idle_timeout: idle,
    cache_lists: f.cacheLists,
  };

  if (f.kind === "command") {
    if (!f.command.trim()) throw new Error("Command is required");
    return {
      name,
      server: {
        ...base,
        command: f.command.trim(),
        url: null,
        transport: null,
        headers: {},
        args: lines(f.args),
        env: parsePairs(f.env, "=", "Environment"),
        cwd: f.cwd.trim() || null,
      },
    };
  }

  if (!f.url.trim()) throw new Error("URL is required");
  if (f.scope === "project")
    throw new Error('Scope "project" needs a local process; use a command server');
  return {
    name,
    server: {
      ...base,
      command: null,
      url: f.url.trim(),
      transport: f.transport === "" ? null : f.transport,
      headers: parsePairs(f.headers, ":", "Headers"),
      args: [],
      env: {},
      cwd: null,
    },
  };
}
