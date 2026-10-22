export type Scope = "shared" | "project" | "session";
export type Transport = "http" | "sse";
export type Status = "unchanged" | "added" | "modified";

export interface ServerConfig {
  enabled: boolean;
  command: string | null;
  wasm: string | null;
  url: string | null;
  transport: Transport | null;
  headers: Record<string, string>;
  args: string[];
  env: Record<string, string>;
  inherit_env: string[];
  dirs: string[];
  cwd: string | null;
  scope: Scope;
  idle_timeout: number | null;
  cache_lists: boolean;
}

export interface Entry {
  name: string;
  server: ServerConfig;
  status: Status;
}

export interface PluginInfo {
  name: string;
  description: string;
  /** The plugin file exists in the plugins directory. */
  installed: boolean;
  /** Draft server entry running the plugin. */
  server: string | null;
}

export interface Snapshot {
  path: string;
  load_error: string | null;
  dirty: boolean;
  entries: Entry[];
  plugins: PluginInfo[];
}

export type ServerStatus =
  | { state: "stopped" }
  | { state: "starting" }
  | { state: "running"; listen: string }
  | { state: "failed"; error: string };

export interface UpdateInfo {
  current: string;
  version: string;
  notes: string | null;
}
