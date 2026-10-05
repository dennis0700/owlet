import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ServerConfig, ServerStatus, Snapshot, UpdateInfo } from "./types";

export const getState = () => invoke<Snapshot>("get_state");
export const reload = () => invoke<Snapshot>("reload");
export const save = () => invoke<Snapshot>("save");
export const setEnabled = (name: string, enabled: boolean) =>
  invoke<Snapshot>("set_enabled", { name, enabled });
export const upsertServer = (
  oldName: string | null,
  name: string,
  server: ServerConfig,
) => invoke<Snapshot>("upsert_server", { oldName, name, server });
export const deleteServer = (name: string) =>
  invoke<Snapshot>("delete_server", { name });

export const serverStatus = () => invoke<ServerStatus>("server_status");
export const startServer = () => invoke<ServerStatus>("start_server");
export const stopServer = () => invoke<ServerStatus>("stop_server");
export const restartServer = () => invoke<ServerStatus>("restart_server");
export const onServerStatus = (handler: (status: ServerStatus) => void) =>
  listen<ServerStatus>("server-status", (e) => handler(e.payload));

export const checkUpdate = () => invoke<UpdateInfo | null>("check_update");
export const installUpdate = () => invoke<void>("install_update");

export const errorMessage = (e: unknown): string =>
  typeof e === "string" ? e : e instanceof Error ? e.message : String(e);
